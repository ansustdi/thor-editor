//! Elasticsearch / OpenSearch over the REST API.
//!
//! Two kinds of statements:
//! * Kibana Dev Tools console requests — `GET logs/_search` followed by an optional JSON body
//!   (several JSON lines are sent as NDJSON, for `_bulk` / `_msearch`).
//! * Anything else is SQL, run through the SQL API (`_sql`, OpenSearch `_plugins/_sql`) with
//!   cursor paging.
//!
//! Responses become rows: search hits (`_id` + `_source` fields), aggregation buckets, `_cat`
//! tables, SQL result sets, or a flattened key/value view of any other JSON.

use super::{CancelFlag, Dialect, RowSink, RunOutcome, Session};
use crate::model::{Cell, ColumnInfo, ColumnMeta, ConnectionProfile, DbObject};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use reqwest::Method;
use serde_json::{json, Map, Value};
use std::time::Duration;

const SQL_FETCH: usize = 1000;
/// Columns are inferred from this many leading documents.
const INFER: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Flavor {
    Elastic,
    OpenSearch,
}

enum Auth {
    None,
    Basic(String, String),
    ApiKey(String),
}

pub struct EsSession {
    http: reqwest::Client,
    base: String,
    auth: Auth,
    flavor: Flavor,
    cluster: String,
    version: String,
    cancel: CancelFlag,
}

/// A parsed console request.
#[derive(Debug, PartialEq)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub body: Body,
}

#[derive(Debug, PartialEq)]
pub enum Body {
    None,
    Json(Value),
    /// Several JSON documents, one per line (`_bulk`, `_msearch`).
    NdJson(Vec<Value>),
}

const METHODS: [&str; 6] = ["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH"];

/// True when the statement is a console request (`METHOD path …`) rather than SQL.
pub fn is_request(stmt: &str) -> bool {
    let first = stmt.trim_start().split_whitespace().next().unwrap_or("");
    METHODS.iter().any(|m| m.eq_ignore_ascii_case(first))
}

/// Kibana's `"""…"""` triple-quoted strings → JSON strings.
fn triple_quotes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("\"\"\"") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 3..];
        match after.find("\"\"\"") {
            Some(j) => {
                out.push_str(&Value::String(after[..j].to_string()).to_string());
                rest = &after[j + 3..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn parse_request(stmt: &str) -> Result<Request> {
    let stmt = stmt.trim().trim_end_matches(';').trim_end();
    let (line, body) = stmt.split_once('\n').unwrap_or((stmt, ""));
    let mut words = line.split_whitespace();
    let method = words.next().unwrap_or("").to_ascii_uppercase();
    let path = words.next().ok_or_else(|| anyhow!("Expected `{method} <path>`, e.g. `GET my-index/_search`"))?;
    if let Some(extra) = words.next() {
        bail!("Unexpected `{extra}` after the path. A request is `{method} <path>` with an optional JSON body on the next lines");
    }
    let method = Method::from_bytes(method.as_bytes())?;
    let path = path.trim_start_matches('/').to_string();
    // Console comments: lines starting with # or //, except inside """…""" strings (scripts, SQL).
    let mut in_triple = false;
    let body: String = body
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            let keep = in_triple || !(t.starts_with('#') || t.starts_with("//"));
            if l.matches("\"\"\"").count() % 2 == 1 {
                in_triple = !in_triple;
            }
            keep
        })
        .collect::<Vec<_>>()
        .join("\n");
    let body = triple_quotes(body.trim());
    let body = if body.is_empty() {
        Body::None
    } else {
        let docs: Vec<Value> = serde_json::Deserializer::from_str(&body)
            .into_iter::<Value>()
            .collect::<std::result::Result<_, _>>()
            .context("Request body is not valid JSON")?;
        match docs.len() {
            1 if !(path.contains("_bulk") || path.contains("_msearch")) => Body::Json(docs.into_iter().next().unwrap()),
            _ => Body::NdJson(docs),
        }
    };
    Ok(Request { method, path, body })
}

/// `{"error":{"type":…,"reason":…,"caused_by":{…}},"status":400}` → readable text.
fn error_text(status: reqwest::StatusCode, body: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        let b = body.trim();
        return if b.is_empty() { format!("HTTP {status}") } else { format!("HTTP {status}: {b}") };
    };
    let e = &v["error"];
    if let Some(s) = e.as_str() {
        return s.to_string();
    }
    let mut parts = vec![];
    let mut cur = e;
    while cur.is_object() {
        let t = cur["type"].as_str().unwrap_or("error");
        let r = cur["reason"].as_str().unwrap_or("");
        let line = if r.is_empty() { t.to_string() } else { format!("{t}: {r}") };
        if !parts.contains(&line) {
            parts.push(line);
        }
        cur = &cur["caused_by"];
    }
    if parts.is_empty() {
        format!("HTTP {status}: {}", body.trim())
    } else {
        parts.join("\nCaused by: ")
    }
}

fn json_cell(v: &Value) -> Cell {
    match v {
        Value::Null => Cell::Null,
        Value::Bool(b) => Cell::Bool(*b),
        Value::Number(n) if n.is_u64() && n.as_i64().is_none() => Cell::Dec(n.to_string()), // unsigned_long > i64::MAX
        Value::Number(n) => n.as_i64().map(Cell::Int).or_else(|| n.as_f64().map(Cell::Float)).unwrap_or(Cell::Dec(n.to_string())),
        Value::String(s) => Cell::Text(s.clone()),
        other => Cell::Text(other.to_string()),
    }
}

fn is_numeric_type(t: &str) -> bool {
    matches!(
        t,
        "long" | "integer" | "short" | "byte" | "double" | "float" | "half_float" | "scaled_float" | "unsigned_long" | "int" | "bigint" | "smallint" | "tinyint" | "real"
    )
}

fn sql_cell(v: &Value, t: &str) -> Cell {
    match (v, t) {
        (Value::String(s), "date" | "datetime" | "timestamp" | "date_nanos") => Cell::DateTime(s.clone()),
        _ => json_cell(v),
    }
}

/// Rows of JSON objects: columns are the union of keys over the first documents.
fn object_rows(rows: &[Map<String, Value>], lead: &[&str], sink: &mut dyn RowSink) -> Result<()> {
    let mut cols: Vec<String> = lead.iter().filter(|c| rows.iter().take(INFER).any(|r| r.contains_key(**c))).map(|c| c.to_string()).collect();
    for r in rows.iter().take(INFER) {
        for k in r.keys() {
            if !cols.contains(k) {
                cols.push(k.clone());
            }
        }
    }
    let numeric = |c: &String| rows.iter().find_map(|r| r.get(c).filter(|v| !v.is_null())).is_some_and(Value::is_number);
    sink.columns(cols.iter().map(|c| ColumnMeta::new(c, "", numeric(c))).collect())?;
    for r in rows {
        if !sink.row(cols.iter().map(|c| r.get(c).map(json_cell).unwrap_or(Cell::Null)).collect())? {
            break;
        }
    }
    Ok(())
}

/// Search hits / mget docs → `_id` (+ `_index` across indices, `_score`) and `_source` fields.
fn hit_rows(hits: &[Value], sink: &mut dyn RowSink) -> Result<()> {
    let first_index = hits.first().and_then(|h| h["_index"].as_str());
    let multi_index = hits.iter().any(|h| h["_index"].as_str() != first_index);
    let scored = hits.iter().any(|h| h["_score"].is_number());
    let rows: Vec<Map<String, Value>> = hits
        .iter()
        .map(|h| {
            let mut m = Map::new();
            m.insert("_id".into(), h["_id"].clone());
            if multi_index {
                m.insert("_index".into(), h["_index"].clone());
            }
            if scored {
                m.insert("_score".into(), h["_score"].clone());
            }
            if let Some(src) = h["_source"].as_object() {
                for (k, v) in src {
                    m.insert(k.clone(), v.clone());
                }
            }
            // Stored / docvalue fields come back as arrays: unwrap single values.
            if let Some(fields) = h["fields"].as_object() {
                for (k, v) in fields {
                    let v = match v {
                        Value::Array(a) if a.len() == 1 => a[0].clone(),
                        other => other.clone(),
                    };
                    m.entry(k.clone()).or_insert(v);
                }
            }
            if let Some(hl) = h.get("highlight").filter(|v| v.is_object()) {
                m.insert("_highlight".into(), hl.clone());
            }
            m
        })
        .collect();
    object_rows(&rows, &["_id", "_index", "_score"], sink)
}

/// Value(s) of a metric aggregation, or None for bucket / complex ones.
fn metric_values(name: &str, agg: &Value, out: &mut Map<String, Value>) -> bool {
    let Some(o) = agg.as_object() else { return false };
    if o.contains_key("buckets") {
        out.insert(name.into(), agg["buckets"].clone());
        return true;
    }
    if let Some(v) = o.get("value") {
        out.insert(name.into(), o.get("value_as_string").cloned().unwrap_or_else(|| v.clone()));
        return true;
    }
    if let Some(Value::Object(vals)) = o.get("values") {
        // percentiles
        for (k, v) in vals {
            out.insert(format!("{name}.{k}"), v.clone());
        }
        return true;
    }
    if let Some(dc) = o.get("doc_count") {
        // single-bucket aggs (filter, missing, nested…): count + their own sub-metrics
        out.insert(format!("{name}.doc_count"), dc.clone());
        for (k, v) in o {
            if k != "doc_count" && k != "meta" {
                metric_values(&format!("{name}.{k}"), v, out);
            }
        }
        return true;
    }
    // stats / extended_stats / top_hits…
    for (k, v) in o {
        if k == "meta" || k.ends_with("_as_string") {
            continue;
        }
        let v = if k == "hits" { v.get("hits").cloned().unwrap_or_else(|| v.clone()) } else { v.clone() };
        out.insert(format!("{name}.{k}"), v);
    }
    true
}

fn bucket_list(buckets: &Value) -> Vec<Value> {
    match buckets {
        Value::Array(a) => a.clone(),
        // keyed buckets (filters, keyed ranges)
        Value::Object(o) => o
            .iter()
            .map(|(k, b)| {
                let mut b = b.clone();
                if let Some(m) = b.as_object_mut() {
                    m.entry("key").or_insert_with(|| Value::String(k.clone()));
                }
                b
            })
            .collect(),
        _ => vec![],
    }
}

/// Aggregations → rows. One bucket aggregation: a row per bucket (key, doc_count, sub-metrics).
/// Otherwise one row with a column per metric.
fn agg_rows(aggs: &Map<String, Value>, sink: &mut dyn RowSink) -> Result<()> {
    if aggs.len() == 1 {
        let (name, agg) = aggs.iter().next().unwrap();
        if let Some(b) = agg.get("buckets") {
            let rows: Vec<Map<String, Value>> = bucket_list(b)
                .iter()
                .map(|b| {
                    let mut m = Map::new();
                    let key = b.get("key_as_string").or_else(|| b.get("key")).cloned().unwrap_or(Value::Null);
                    m.insert(name.clone(), key);
                    m.insert("doc_count".into(), b["doc_count"].clone());
                    if let Some(o) = b.as_object() {
                        for (k, v) in o {
                            if !matches!(k.as_str(), "key" | "key_as_string" | "doc_count" | "from" | "to" | "from_as_string" | "to_as_string") {
                                metric_values(k, v, &mut m);
                            }
                        }
                    }
                    m
                })
                .collect();
            return object_rows(&rows, &[name.as_str(), "doc_count"], sink);
        }
    }
    let mut m = Map::new();
    for (k, v) in aggs {
        metric_values(k, v, &mut m);
    }
    object_rows(&[m], &[], sink)
}

/// Any JSON → (key, value) rows with dotted paths.
fn flatten(prefix: &str, v: &Value, out: &mut Vec<(String, Value)>) {
    match v {
        Value::Object(o) if !o.is_empty() => {
            for (k, v) in o {
                let p = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten(&p, v, out);
            }
        }
        Value::Array(a) if a.iter().any(|x| x.is_object()) => {
            for (i, v) in a.iter().enumerate() {
                flatten(&format!("{prefix}[{i}]"), v, out);
            }
        }
        _ => out.push((prefix.to_string(), v.clone())),
    }
}

fn is_hit(v: &Value) -> bool {
    v.get("_id").is_some() && v.get("_index").is_some()
}

/// Picks the best tabular shape for a REST response.
fn shape(v: &Value, sink: &mut dyn RowSink) -> Result<()> {
    match v {
        Value::Array(a) if !a.is_empty() && a.iter().all(Value::is_object) => {
            let rows: Vec<Map<String, Value>> = a.iter().filter_map(|x| x.as_object().cloned()).collect();
            return object_rows(&rows, &[], sink);
        }
        Value::Object(o) => {
            // SQL API responses posted through the console.
            if let (Some(Value::Array(cols)), Some(Value::Array(rows))) = (o.get("columns").or(o.get("schema")), o.get("rows").or(o.get("datarows"))) {
                return sql_page_columns(cols, sink).and_then(|types| {
                    for r in rows {
                        if !sink.row(sql_row(r, &types))? {
                            break;
                        }
                    }
                    Ok(())
                });
            }
            let hits = o.get("hits").and_then(|h| h.get("hits")).and_then(Value::as_array);
            let aggs = o.get("aggregations").and_then(Value::as_object);
            if let Some(aggs) = aggs.filter(|a| !a.is_empty() && hits.is_none_or(|h| h.is_empty())) {
                return agg_rows(aggs, sink);
            }
            if let Some(hits) = hits {
                if hits.is_empty() {
                    sink.columns(vec![ColumnMeta::new("_id", "", false)])?;
                    return Ok(());
                }
                return hit_rows(hits, sink);
            }
            if let Some(Value::Array(docs)) = o.get("docs") {
                if docs.iter().all(is_hit) {
                    return hit_rows(docs, sink);
                }
            }
            if let Some(Value::Array(rs)) = o.get("responses") {
                // _msearch: hits of every response, one after another.
                let all: Vec<Value> = rs.iter().filter_map(|r| r["hits"]["hits"].as_array()).flatten().cloned().collect();
                if !all.is_empty() {
                    return hit_rows(&all, sink);
                }
            }
            if is_hit(v) && v.get("_source").is_some() {
                return hit_rows(std::slice::from_ref(v), sink);
            }
        }
        _ => {}
    }
    let mut kv = vec![];
    flatten("", v, &mut kv);
    sink.columns(vec![ColumnMeta::new("key", "", false), ColumnMeta::new("value", "", false)])?;
    for (k, v) in kv {
        if !sink.row(vec![Cell::Text(k), json_cell(&v)])? {
            break;
        }
    }
    Ok(())
}

/// SQL API column descriptors → declared to the sink; returns the type names.
fn sql_page_columns(cols: &[Value], sink: &mut dyn RowSink) -> Result<Vec<String>> {
    let types: Vec<String> = cols.iter().map(|c| c["type"].as_str().unwrap_or("").to_string()).collect();
    sink.columns(
        cols.iter()
            .zip(&types)
            .map(|(c, t)| ColumnMeta::new(c["alias"].as_str().or(c["name"].as_str()).unwrap_or(""), t, is_numeric_type(t)))
            .collect(),
    )?;
    Ok(types)
}

fn sql_row(r: &Value, types: &[String]) -> Vec<Cell> {
    match r.as_array() {
        Some(a) => a.iter().zip(types).map(|(v, t)| sql_cell(v, t)).collect(),
        None => vec![json_cell(r)],
    }
}

impl EsSession {
    pub async fn connect(p: &ConnectionProfile, password: &str) -> Result<Self> {
        let host = if p.host.is_empty() { "localhost" } else { p.host.trim() };
        let tls = matches!(p.ssl_mode.as_str(), "require" | "verify-full");
        let http = reqwest::Client::builder()
            .danger_accept_invalid_certs(p.ssl_mode == "require")
            .connect_timeout(Duration::from_secs(15))
            .build()?;
        // Host may be a full URL (Elastic Cloud endpoints); the port field is then ignored.
        let base = if host.starts_with("http://") || host.starts_with("https://") {
            host.trim_end_matches('/').to_string()
        } else {
            format!("{}://{host}:{}", if tls { "https" } else { "http" }, p.port_or_default())
        };
        let auth = match (p.username.is_empty(), password.is_empty()) {
            (false, _) => Auth::Basic(p.username.clone(), password.to_string()),
            (true, false) => Auth::ApiKey(password.to_string()),
            (true, true) => Auth::None,
        };
        let mut s = Self {
            http,
            base,
            auth,
            flavor: Flavor::Elastic,
            cluster: String::new(),
            version: String::new(),
            cancel: CancelFlag::default(),
        };
        let root = s.call(Method::GET, "", &Body::None).await.context("Elasticsearch connect failed")?;
        let ver = &root["version"];
        s.flavor = if ver["distribution"].as_str() == Some("opensearch") { Flavor::OpenSearch } else { Flavor::Elastic };
        s.cluster = root["cluster_name"].as_str().unwrap_or("cluster").to_string();
        s.version = format!(
            "{} {}",
            if s.flavor == Flavor::OpenSearch { "OpenSearch" } else { "Elasticsearch" },
            ver["number"].as_str().unwrap_or("?")
        );
        Ok(s)
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }

    /// Sends a request and returns the raw status + body. Only query requests are `cancellable`:
    /// the flag stays set after a cancel, and metadata calls must not trip over it.
    async fn send(&self, method: Method, path: &str, body: &Body, cancellable: bool) -> Result<(reqwest::StatusCode, String)> {
        let mut path = path.to_string();
        // _cat APIs answer in text tables unless asked for JSON.
        if path.starts_with("_cat") && !path.contains("format=") {
            path.push_str(if path.contains('?') { "&format=json" } else { "?format=json" });
        }
        let mut req = self.http.request(method, self.url(&path));
        req = match &self.auth {
            Auth::None => req,
            Auth::Basic(u, p) => req.basic_auth(u, Some(p)),
            Auth::ApiKey(k) => req.header("Authorization", format!("ApiKey {k}")),
        };
        req = match body {
            Body::None => req,
            Body::Json(v) => req.header("Content-Type", "application/json").body(v.to_string()),
            Body::NdJson(docs) => {
                let mut s = String::new();
                for d in docs {
                    s.push_str(&d.to_string());
                    s.push('\n');
                }
                req.header("Content-Type", "application/x-ndjson").body(s)
            }
        };
        let fut = async {
            let r = req.send().await.with_context(|| format!("Request to {} failed", self.base))?;
            let status = r.status();
            let text = r.text().await?;
            Ok::<_, anyhow::Error>((status, text))
        };
        if !cancellable {
            return fut.await;
        }
        tokio::pin!(fut);
        loop {
            tokio::select! {
                r = &mut fut => return r,
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    if self.cancel.is_set() {
                        bail!("Cancelled");
                    }
                }
            }
        }
    }

    fn parse(status: reqwest::StatusCode, text: String) -> Result<Value> {
        if !status.is_success() {
            bail!("{}", error_text(status, &text));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    /// Metadata / housekeeping request; HTTP errors become readable messages.
    async fn call(&self, method: Method, path: &str, body: &Body) -> Result<Value> {
        let (status, text) = self.send(method, path, body, false).await?;
        Self::parse(status, text)
    }

    /// User query request: aborted by Cancel.
    async fn query(&self, method: Method, path: &str, body: &Body) -> Result<Value> {
        let (status, text) = self.send(method, path, body, true).await?;
        Self::parse(status, text)
    }

    async fn run_request(&self, req: Request, sink: &mut dyn RowSink) -> Result<RunOutcome> {
        if req.method == Method::HEAD {
            let (status, _) = self.send(req.method, &req.path, &Body::None, true).await?;
            sink.columns(vec![ColumnMeta::new("status", "", true), ColumnMeta::new("exists", "", false)])?;
            sink.row(vec![Cell::Int(status.as_u16() as i64), Cell::Bool(status.is_success())])?;
            return Ok(RunOutcome::default());
        }
        let v = self.query(req.method, &req.path, &req.body).await?;
        shape(&v, sink)?;
        Ok(RunOutcome::default())
    }

    fn sql_path(&self) -> (&'static str, &'static str) {
        match self.flavor {
            Flavor::Elastic => ("_sql?format=json", "_sql/close"),
            Flavor::OpenSearch => ("_plugins/_sql?format=jdbc", "_plugins/_sql/close"),
        }
    }

    async fn run_sql(&self, sql: &str, sink: &mut dyn RowSink) -> Result<RunOutcome> {
        let (path, close) = self.sql_path();
        let first = self.query(Method::POST, path, &Body::Json(json!({ "query": sql, "fetch_size": SQL_FETCH }))).await?;
        let mut cursor = None;
        let r = self.sql_pages(path, first, sink, &mut cursor).await;
        // A cursor still open here (stopped early, cancelled or failed) holds a search context on
        // the cluster until it times out: close it.
        if let Some(c) = cursor {
            let _ = self.call(Method::POST, close, &Body::Json(json!({ "cursor": c }))).await;
        }
        r
    }

    /// Streams SQL result pages; `cursor` always holds the cursor that is still open.
    async fn sql_pages(&self, path: &str, mut page: Value, sink: &mut dyn RowSink, cursor: &mut Option<String>) -> Result<RunOutcome> {
        let cols = page.get("columns").or(page.get("schema")).and_then(Value::as_array).cloned().unwrap_or_default();
        let types = sql_page_columns(&cols, sink)?;
        loop {
            let rows = page.get("rows").or(page.get("datarows")).and_then(Value::as_array).cloned().unwrap_or_default();
            *cursor = page.get("cursor").and_then(Value::as_str).filter(|c| !c.is_empty()).map(String::from);
            for r in &rows {
                if !sink.row(sql_row(r, &types))? || self.cancel.is_set() {
                    return Ok(RunOutcome::default());
                }
            }
            match cursor.clone() {
                Some(c) if !rows.is_empty() => page = self.query(Method::POST, path, &Body::Json(json!({ "cursor": c }))).await?,
                _ => return Ok(RunOutcome::default()),
            }
        }
    }

    /// Index / alias / data stream names, without system (dot-prefixed) ones.
    async fn names(&self, path: &str, field: &str) -> Vec<String> {
        let Ok(v) = self.call(Method::GET, path, &Body::None).await else { return vec![] };
        let mut out: Vec<String> = v
            .as_array()
            .map(|a| a.iter().filter_map(|x| x[field].as_str()).filter(|n| !n.starts_with('.')).map(String::from).collect())
            .unwrap_or_default();
        out.sort();
        out.dedup();
        out
    }
}

/// Mapping properties → dotted field names with their types (objects recurse, multi-fields included).
fn mapping_fields(prefix: &str, props: &Map<String, Value>, out: &mut Vec<(String, String)>) {
    for (k, def) in props {
        let name = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        let sub = def.get("properties").and_then(Value::as_object);
        let t = def["type"].as_str().unwrap_or(if sub.is_some() { "object" } else { "unknown" });
        out.push((name.clone(), t.to_string()));
        if let Some(sub) = sub {
            mapping_fields(&name, sub, out);
        }
        if let Some(Value::Object(fields)) = def.get("fields") {
            for (f, fd) in fields {
                out.push((format!("{name}.{f}"), fd["type"].as_str().unwrap_or("unknown").to_string()));
            }
        }
    }
}

/// Settings Elasticsearch generates itself; they cannot be passed to `PUT index`.
const GENERATED_SETTINGS: [&str; 5] = ["uuid", "creation_date", "version", "provided_name", "history_uuid"];

#[async_trait]
impl Session for EsSession {
    fn dialect(&self) -> Dialect {
        Dialect::Elastic
    }
    fn cancel_flag(&self) -> &CancelFlag {
        &self.cancel
    }

    async fn run(&self, stmt: &str, sink: &mut dyn RowSink) -> Result<RunOutcome> {
        self.cancel.reset();
        if is_request(stmt) {
            self.run_request(parse_request(stmt)?, sink).await
        } else {
            self.run_sql(stmt.trim().trim_end_matches(';'), sink).await
        }
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
        Ok(self.version.clone())
    }

    async fn native_current_schema(&self) -> Result<Option<String>> {
        Ok(Some(self.cluster.clone()))
    }

    async fn native_schemas(&self) -> Result<Option<Vec<String>>> {
        Ok(Some(vec![self.cluster.clone()]))
    }

    async fn native_objects(&self, schema: &str) -> Result<Option<Vec<DbObject>>> {
        // Fail loudly if the cluster is unreachable; the optional lists below fail quietly.
        let indices = self.call(Method::GET, "_cat/indices?format=json&h=index", &Body::None).await?;
        let mut out = vec![];
        let mut push = |names: Vec<String>, kind: &str| {
            out.extend(names.into_iter().map(|name| DbObject { schema: schema.to_string(), name, kind: kind.into() }));
        };
        let mut idx: Vec<String> = indices
            .as_array()
            .map(|a| a.iter().filter_map(|x| x["index"].as_str()).filter(|n| !n.starts_with('.')).map(String::from).collect())
            .unwrap_or_default();
        idx.sort();
        push(idx, "es index");
        push(self.names("_cat/aliases?format=json&h=alias", "alias").await, "alias");
        let ds = match self.call(Method::GET, "_data_stream", &Body::None).await {
            Ok(v) => v["data_streams"]
                .as_array()
                .map(|a| a.iter().filter_map(|d| d["name"].as_str()).filter(|n| !n.starts_with('.')).map(String::from).collect())
                .unwrap_or_default(),
            Err(_) => vec![],
        };
        push(ds, "data stream");
        Ok(Some(out))
    }

    async fn native_columns(&self, _schema: &str, name: &str) -> Result<Option<Vec<ColumnInfo>>> {
        let v = self.call(Method::GET, &format!("{name}/_mapping"), &Body::None).await?;
        let mut fields: Vec<(String, String)> = vec![("_id".into(), "keyword".into())];
        // Aliases and data streams map to several indices: merge their fields.
        for idx in v.as_object().into_iter().flat_map(|o| o.values()) {
            let mut f = vec![];
            if let Some(props) = idx["mappings"]["properties"].as_object() {
                mapping_fields("", props, &mut f);
            }
            for (n, t) in f {
                match fields.iter_mut().find(|(x, _)| *x == n) {
                    Some((_, ty)) if !ty.split(" | ").any(|x| x == t) => *ty = format!("{ty} | {t}"),
                    Some(_) => {}
                    None => fields.push((n, t)),
                }
            }
        }
        Ok(Some(
            fields
                .into_iter()
                .map(|(n, t)| ColumnInfo { primary_key: n == "_id", name: n, data_type: t, nullable: true, default_value: None, comment: None })
                .collect(),
        ))
    }

    async fn native_ddl(&self, _schema: &str, name: &str, kind: &str) -> Result<Option<String>> {
        if kind == "data stream" {
            let v = self.call(Method::GET, &format!("_data_stream/{name}"), &Body::None).await?;
            let tpl = v["data_streams"][0]["template"].as_str().unwrap_or("").to_string();
            let mut out = format!("# Data stream {name} (index template: {tpl})\nGET _data_stream/{name}\n");
            if !tpl.is_empty() {
                let t = self.call(Method::GET, &format!("_index_template/{tpl}"), &Body::None).await?;
                if let Some(body) = t["index_templates"][0].get("index_template") {
                    out.push_str(&format!("\nPUT _index_template/{tpl}\n{}\n", serde_json::to_string_pretty(body)?));
                }
            }
            return Ok(Some(out));
        }
        let v = self.call(Method::GET, name, &Body::None).await?;
        let mut out = String::new();
        for (idx, def) in v.as_object().into_iter().flat_map(|o| o.iter()) {
            let mut body = Map::new();
            if def["aliases"].as_object().is_some_and(|a| !a.is_empty()) {
                body.insert("aliases".into(), def["aliases"].clone());
            }
            let mut settings = def["settings"]["index"].clone();
            if let Some(s) = settings.as_object_mut() {
                for k in GENERATED_SETTINGS {
                    s.remove(k);
                }
            }
            body.insert("settings".into(), json!({ "index": settings }));
            body.insert("mappings".into(), def["mappings"].clone());
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("PUT {idx}\n{}\n", serde_json::to_string_pretty(&Value::Object(body))?));
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::CollectSink;

    #[test]
    fn parses_console_requests() {
        assert!(is_request("get logs/_search"));
        assert!(!is_request("SELECT * FROM logs"));
        let r = parse_request("GET /logs/_search\n# comment\n{\"query\": {\"match_all\": {}}};").unwrap();
        assert_eq!(r.method, Method::GET);
        assert_eq!(r.path, "logs/_search");
        assert_eq!(r.body, Body::Json(json!({"query": {"match_all": {}}})));
        let r = parse_request("POST _bulk\n{\"index\":{\"_index\":\"a\"}}\n{\"x\":1}").unwrap();
        assert_eq!(r.body, Body::NdJson(vec![json!({"index":{"_index":"a"}}), json!({"x":1})]));
        let r = parse_request("POST _sql\n{\"query\": \"\"\"SELECT \"a\" FROM t\"\"\"}").unwrap();
        assert_eq!(r.body, Body::Json(json!({"query": "SELECT \"a\" FROM t"})));
        let r = parse_request("POST _scripts/s\n{\"script\": {\"lang\": \"painless\", \"source\": \"\"\"\n// kept\nreturn 1;\n\"\"\"}}").unwrap();
        assert_eq!(r.body, Body::Json(json!({"script": {"lang": "painless", "source": "\n// kept\nreturn 1;\n"}})));
        assert!(parse_request("delete from logs").is_err());
        assert!(parse_request("GET").is_err());
    }

    #[test]
    fn shapes_hits_and_aggs() {
        let mut s = CollectSink::new(0);
        shape(
            &json!({"hits": {"hits": [
                {"_index": "a", "_id": "1", "_score": 1.0, "_source": {"name": "x", "n": 1}},
                {"_index": "a", "_id": "2", "_score": 0.5, "_source": {"name": "y", "tags": ["p"]}}
            ]}}),
            &mut s,
        )
        .unwrap();
        let cols: Vec<&str> = s.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(cols, vec!["_id", "_score", "name", "n", "tags"]);
        assert_eq!(s.rows[1][4], Cell::Text("[\"p\"]".into()));

        let mut s = CollectSink::new(0);
        shape(
            &json!({"hits": {"hits": []}, "aggregations": {"by_city": {"buckets": [
                {"key": "tokyo", "doc_count": 3, "avg_age": {"value": 30.5}},
                {"key": "ub", "doc_count": 2, "avg_age": {"value": null}}
            ]}}}),
            &mut s,
        )
        .unwrap();
        let cols: Vec<&str> = s.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(cols, vec!["by_city", "doc_count", "avg_age"]);
        assert_eq!(s.rows[0], vec![Cell::Text("tokyo".into()), Cell::Int(3), Cell::Float(30.5)]);

        let mut s = CollectSink::new(0);
        shape(&json!({"cluster_name": "c", "nodes": {"total": 1}}), &mut s).unwrap();
        assert_eq!(s.rows[1], vec![Cell::Text("nodes.total".into()), Cell::Int(1)]);
    }

    #[test]
    fn readable_errors() {
        let body = r#"{"error":{"root_cause":[],"type":"search_phase_execution_exception","reason":"all shards failed",
            "caused_by":{"type":"illegal_argument_exception","reason":"Text fields are not optimised"}},"status":400}"#;
        assert_eq!(
            error_text(reqwest::StatusCode::BAD_REQUEST, body),
            "search_phase_execution_exception: all shards failed\nCaused by: illegal_argument_exception: Text fields are not optimised"
        );
    }

    #[test]
    fn flattens_mappings() {
        let props = json!({"name": {"type": "text", "fields": {"keyword": {"type": "keyword"}}}, "addr": {"properties": {"city": {"type": "keyword"}}}});
        let mut f = vec![];
        mapping_fields("", props.as_object().unwrap(), &mut f);
        assert!(f.contains(&("name.keyword".into(), "keyword".into())));
        assert!(f.contains(&("addr".into(), "object".into())));
        assert!(f.contains(&("addr.city".into(), "keyword".into())));
    }
}
