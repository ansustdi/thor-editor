//! Neo4j over Bolt (Cypher). Nodes/relationships are returned as JSON cells with
//! `~id`/`~labels`/`~type`/`~start`/`~end` markers so the UI can draw a graph.

use super::{CancelFlag, Dialect, RowSink, RunOutcome, Session};
use crate::model::{Cell, ColumnInfo, ColumnMeta, ConnectionProfile, DbObject};
use anyhow::{Context, Result};
use async_trait::async_trait;
use neo4rs::{query, BoltType, ConfigBuilder, Graph};
use serde_json::{json, Map, Value};
use std::sync::Mutex;

pub struct NeoSession {
    graph: Graph,
    db: Mutex<String>,
    cancel: CancelFlag,
}

fn bolt_json(b: &BoltType) -> Value {
    match b {
        BoltType::Null(_) => Value::Null,
        BoltType::Boolean(v) => json!(v.value),
        BoltType::Integer(v) => json!(v.value),
        BoltType::Float(v) => json!(v.value),
        BoltType::String(v) => json!(v.value),
        BoltType::Bytes(v) => json!(hex::encode(&v.value)),
        BoltType::List(l) => Value::Array(l.value.iter().map(bolt_json).collect()),
        BoltType::Map(m) => Value::Object(m.value.iter().map(|(k, v)| (k.value.clone(), bolt_json(v))).collect()),
        BoltType::Node(n) => {
            let mut o = Map::new();
            o.insert("~id".into(), json!(n.id.value));
            o.insert("~labels".into(), Value::Array(n.labels.value.iter().map(bolt_json).collect()));
            for (k, v) in &n.properties.value {
                o.insert(k.value.clone(), bolt_json(v));
            }
            Value::Object(o)
        }
        BoltType::Relation(r) => {
            let mut o = Map::new();
            o.insert("~id".into(), json!(r.id.value));
            o.insert("~type".into(), json!(r.typ.value));
            o.insert("~start".into(), json!(r.start_node_id.value));
            o.insert("~end".into(), json!(r.end_node_id.value));
            for (k, v) in &r.properties.value {
                o.insert(k.value.clone(), bolt_json(v));
            }
            Value::Object(o)
        }
        BoltType::UnboundedRelation(r) => {
            let mut o = Map::new();
            o.insert("~id".into(), json!(r.id.value));
            o.insert("~type".into(), json!(r.typ.value));
            for (k, v) in &r.properties.value {
                o.insert(k.value.clone(), bolt_json(v));
            }
            Value::Object(o)
        }
        BoltType::Path(p) => json!({
            "~path": p.nodes.value.iter().map(bolt_json).collect::<Vec<_>>(),
            "~rels": p.rels.value.iter().map(bolt_json).collect::<Vec<_>>(),
        }),
        other => json!(format!("{other:?}")),
    }
}

fn bolt_cell(b: &BoltType) -> Cell {
    match b {
        BoltType::Null(_) => Cell::Null,
        BoltType::Boolean(v) => Cell::Bool(v.value),
        BoltType::Integer(v) => Cell::Int(v.value),
        BoltType::Float(v) => Cell::Float(v.value),
        BoltType::String(v) => Cell::Text(v.value.clone()),
        BoltType::Bytes(v) => Cell::Bytes(v.value.to_vec()),
        BoltType::Date(_) | BoltType::Time(_) | BoltType::LocalTime(_) | BoltType::DateTime(_)
        | BoltType::LocalDateTime(_) | BoltType::DateTimeZoneId(_) => Cell::DateTime(temporal_text(b)),
        other => Cell::Text(bolt_json(other).to_string()),
    }
}

fn temporal_text(b: &BoltType) -> String {
    use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
    let r: Option<String> = match b {
        BoltType::Date(d) => NaiveDate::try_from(d).ok().map(|x| x.to_string()),
        BoltType::LocalDateTime(d) => NaiveDateTime::try_from(d).ok().map(|x| x.to_string()),
        BoltType::DateTime(d) => DateTime::<FixedOffset>::try_from(d).ok().map(|x| x.to_rfc3339()),
        BoltType::LocalTime(t) => Some(NaiveTime::from(t).to_string()),
        _ => None,
    };
    r.unwrap_or_else(|| format!("{b:?}"))
}

/// Column names in RETURN order (the driver hands rows back as unordered maps).
pub fn return_columns(cypher: &str) -> Vec<String> {
    let low = cypher.to_ascii_lowercase();
    let Some(pos) = find_last_keyword(&low, "return") else { return vec![] };
    let mut rest = &cypher[pos + 6..];
    let rest_low = rest.to_ascii_lowercase();
    for kw in [" order by ", " skip ", " limit ", "\norder by", "\nskip", "\nlimit", " union "] {
        if let Some(i) = rest_low.find(kw) {
            rest = &rest[..i.min(rest.len())];
        }
    }
    let rest = rest.trim().trim_end_matches(';');
    let rest = rest.strip_prefix("DISTINCT ").or_else(|| rest.strip_prefix("distinct ")).unwrap_or(rest);
    let mut cols = vec![];
    let (mut depth, mut cur, mut quote) = (0i32, String::new(), None::<char>);
    for c in rest.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                cur.push(c)
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"' | '`') => {
                quote = Some(c);
                cur.push(c)
            }
            (None, '(' | '[' | '{') => {
                depth += 1;
                cur.push(c)
            }
            (None, ')' | ']' | '}') => {
                depth -= 1;
                cur.push(c)
            }
            (None, ',') if depth == 0 => cols.push(std::mem::take(&mut cur)),
            (None, c) => cur.push(c),
        }
    }
    cols.push(cur);
    cols.into_iter()
        .map(|c| {
            let c = c.trim();
            let lc = c.to_ascii_lowercase();
            match lc.rfind(" as ") {
                Some(i) => c[i + 4..].trim().trim_matches('`').to_string(),
                None => c.to_string(),
            }
        })
        .filter(|c| !c.is_empty() && c != "*")
        .collect()
}

fn find_last_keyword(low: &str, kw: &str) -> Option<usize> {
    let bytes = low.as_bytes();
    let mut found = None;
    let mut start = 0;
    while let Some(i) = low[start..].find(kw) {
        let at = start + i;
        let before = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
        let after = bytes.get(at + kw.len()).map(|b| !b.is_ascii_alphanumeric()).unwrap_or(true);
        if before && after {
            found = Some(at);
        }
        start = at + kw.len();
    }
    found
}

impl NeoSession {
    pub async fn connect(p: &ConnectionProfile, password: &str) -> Result<Self> {
        let host = if p.host.is_empty() { "localhost" } else { &p.host };
        let scheme = match p.ssl_mode.as_str() {
            "require" => "neo4j+ssc",
            "verify-full" => "neo4j+s",
            _ => "neo4j",
        };
        let db = if p.database.is_empty() { "neo4j".to_string() } else { p.database.clone() };
        let cfg = ConfigBuilder::default()
            .uri(format!("{scheme}://{host}:{}", p.port_or_default()))
            .user(p.username.clone())
            .password(password.to_string())
            .db(db.as_str())
            .fetch_size(500)
            .build()?;
        let graph = Graph::connect(cfg).await.context("Neo4j connect failed")?;
        let s = Self { graph, db: Mutex::new(db), cancel: CancelFlag::default() };
        s.server_version().await.context("Neo4j handshake failed")?;
        Ok(s)
    }

    async fn rows_on(&self, db: &str, cypher: &str) -> Result<Vec<neo4rs::Row>> {
        let mut st = self.graph.execute_on(db, query(cypher)).await?;
        let mut out = vec![];
        while let Some(r) = st.next().await? {
            out.push(r);
        }
        Ok(out)
    }
}

#[async_trait]
impl Session for NeoSession {
    fn dialect(&self) -> Dialect {
        Dialect::Neo4j
    }
    fn cancel_flag(&self) -> &CancelFlag {
        &self.cancel
    }

    async fn run(&self, cypher: &str, sink: &mut dyn RowSink) -> Result<RunOutcome> {
        self.cancel.reset();
        let cypher = cypher.trim().trim_end_matches(';');
        if let Some(db) = cypher.strip_prefix(":use ").or_else(|| cypher.strip_prefix(":USE ")) {
            *self.db.lock().unwrap() = db.trim().to_string();
            return Ok(RunOutcome { affected: Some(0) });
        }
        let db = self.db.lock().unwrap().clone();
        let mut stream = self.graph.execute_on(db.as_str(), query(cypher)).await?;
        let mut cols: Option<Vec<String>> = None;
        let parsed = return_columns(cypher);
        while let Some(row) = stream.next().await? {
            let map: neo4rs::BoltMap = row.to()?;
            if cols.is_none() {
                let keys: Vec<String> = map.value.keys().map(|k| k.value.clone()).collect();
                let mut ordered: Vec<String> = parsed.iter().filter(|c| keys.contains(c)).cloned().collect();
                let mut rest: Vec<String> = keys.into_iter().filter(|k| !ordered.contains(k)).collect();
                rest.sort();
                ordered.extend(rest);
                sink.columns(ordered.iter().map(|c| ColumnMeta::new(c, "", false)).collect())?;
                cols = Some(ordered);
            }
            let cells = cols
                .as_ref()
                .unwrap()
                .iter()
                .map(|c| map.value.get(&neo4rs::BoltString::from(c.as_str())).map(bolt_cell).unwrap_or(Cell::Null))
                .collect();
            if !sink.row(cells)? || self.cancel.is_set() {
                break;
            }
        }
        if cols.is_none() {
            if parsed.is_empty() {
                return Ok(RunOutcome { affected: Some(0) });
            }
            sink.columns(parsed.iter().map(|c| ColumnMeta::new(c, "", false)).collect())?;
        }
        Ok(RunOutcome::default())
    }

    async fn set_autocommit(&self, _on: bool) -> Result<()> {
        Ok(())
    }
    async fn commit(&self) -> Result<()> {
        Ok(())
    }
    async fn rollback(&self) -> Result<()> {
        Ok(())
    }

    async fn server_version(&self) -> Result<String> {
        let rows = self
            .rows_on("system", "CALL dbms.components() YIELD name, versions, edition RETURN name, versions[0] AS v, edition")
            .await?;
        Ok(rows
            .first()
            .map(|r| {
                format!(
                    "{} {} ({})",
                    r.get::<String>("name").unwrap_or_default(),
                    r.get::<String>("v").unwrap_or_default(),
                    r.get::<String>("edition").unwrap_or_default()
                )
            })
            .unwrap_or_else(|| "Neo4j".into()))
    }

    async fn native_current_schema(&self) -> Result<Option<String>> {
        Ok(Some(self.db.lock().unwrap().clone()))
    }

    async fn native_schemas(&self) -> Result<Option<Vec<String>>> {
        let cur = self.db.lock().unwrap().clone();
        let mut names: Vec<String> = match self.rows_on("system", "SHOW DATABASES YIELD name RETURN DISTINCT name").await {
            Ok(rows) => rows.iter().filter_map(|r| r.get::<String>("name").ok()).filter(|n| n != "system").collect(),
            Err(_) => vec![],
        };
        if !names.contains(&cur) {
            names.push(cur);
        }
        names.sort();
        Ok(Some(names))
    }

    /// Browsing a database makes it the session database (like `:use`).
    async fn native_objects(&self, schema: &str) -> Result<Option<Vec<DbObject>>> {
        *self.db.lock().unwrap() = schema.to_string();
        let mut out = vec![];
        for r in self.rows_on(schema, "CALL db.labels() YIELD label RETURN label ORDER BY label").await? {
            out.push(DbObject { schema: schema.into(), name: r.get("label")?, kind: "label".into() });
        }
        for r in self
            .rows_on(schema, "CALL db.relationshipTypes() YIELD relationshipType RETURN relationshipType ORDER BY relationshipType")
            .await?
        {
            out.push(DbObject { schema: schema.into(), name: r.get("relationshipType")?, kind: "relationship".into() });
        }
        Ok(Some(out))
    }

    async fn native_columns(&self, schema: &str, name: &str) -> Result<Option<Vec<ColumnInfo>>> {
        let n = name.replace('`', "``");
        let mut rows = self
            .rows_on(schema, &format!("MATCH (n:`{n}`) WITH n LIMIT 200 UNWIND keys(n) AS k RETURN DISTINCT k ORDER BY k"))
            .await?;
        if rows.is_empty() {
            rows = self
                .rows_on(schema, &format!("MATCH ()-[r:`{n}`]->() WITH r LIMIT 200 UNWIND keys(r) AS k RETURN DISTINCT k ORDER BY k"))
                .await?;
        }
        Ok(Some(
            rows.iter()
                .filter_map(|r| r.get::<String>("k").ok())
                .map(|k| ColumnInfo { name: k, data_type: "property".into(), nullable: true, default_value: None, primary_key: false, comment: None })
                .collect(),
        ))
    }

    async fn graph_edges(&self, ids: &[String], limit: usize) -> Result<Vec<Value>> {
        let ids: Vec<i64> = ids.iter().filter_map(|i| i.parse().ok()).collect();
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let db = self.db.lock().unwrap().clone();
        let q = query("MATCH (a)-[r]->(b) WHERE id(a) IN $ids AND id(b) IN $ids RETURN r LIMIT $n")
            .param("ids", ids)
            .param("n", limit as i64);
        let mut st = self.graph.execute_on(db.as_str(), q).await?;
        let mut out = vec![];
        while let Some(row) = st.next().await? {
            let map: neo4rs::BoltMap = row.to()?;
            if let Some(r) = map.value.get(&neo4rs::BoltString::from("r")) {
                out.push(bolt_json(r));
            }
        }
        Ok(out)
    }

    async fn native_ddl(&self, schema: &str, name: &str, _kind: &str) -> Result<Option<String>> {
        let mut out = String::new();
        for (what, q) in [
            ("indexes", "SHOW INDEXES YIELD createStatement, labelsOrTypes RETURN createStatement, labelsOrTypes"),
            ("constraints", "SHOW CONSTRAINTS YIELD createStatement, labelsOrTypes RETURN createStatement, labelsOrTypes"),
        ] {
            match self.rows_on(schema, q).await {
                Ok(rows) => {
                    for r in rows {
                        let on: Vec<String> = r.get("labelsOrTypes").unwrap_or_default();
                        if on.iter().any(|l| l == name) {
                            if let Ok(s) = r.get::<String>("createStatement") {
                                out.push_str(&s);
                                out.push_str(";\n");
                            }
                        }
                    }
                }
                Err(e) => out.push_str(&format!("// {what} unavailable: {e}\n")),
            }
        }
        if out.is_empty() {
            out = format!("// No indexes or constraints on :{name}");
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::return_columns;

    #[test]
    fn return_order() {
        assert_eq!(return_columns("MATCH (n)-[r]->(m) RETURN n, r, m.name AS name LIMIT 5"), vec!["n", "r", "name"]);
        assert_eq!(return_columns("MATCH (n) RETURN count(n), collect(n.a, n.b) ORDER BY 1"), vec!["count(n)", "collect(n.a, n.b)"]);
        assert!(return_columns("CREATE (n:X)").is_empty());
    }
}
