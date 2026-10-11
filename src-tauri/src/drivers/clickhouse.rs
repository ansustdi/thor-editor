//! ClickHouse over its HTTP interface, streaming `JSONCompactEachRowWithNamesAndTypes`.

use super::{lit, CancelFlag, Dialect, RowSink, RunOutcome, Session};
use crate::model::{Cell, ColumnMeta, ConnectionProfile};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;
use std::sync::Mutex;

pub struct ChSession {
    http: reqwest::Client,
    base: String,
    user: String,
    password: String,
    database: Mutex<String>,
    running: Mutex<Option<String>>,
    cancel: CancelFlag,
}

impl ChSession {
    pub async fn connect(p: &ConnectionProfile, password: &str) -> Result<Self> {
        let tls = matches!(p.ssl_mode.as_str(), "require" | "verify-full");
        let host = if p.host.is_empty() { "localhost" } else { &p.host };
        let http = reqwest::Client::builder()
            .danger_accept_invalid_certs(p.ssl_mode == "require")
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()?;
        let s = Self {
            http,
            base: format!("{}://{host}:{}/", if tls { "https" } else { "http" }, p.port_or_default()),
            user: if p.username.is_empty() { "default".into() } else { p.username.clone() },
            password: password.to_string(),
            database: Mutex::new(if p.database.is_empty() { "default".into() } else { p.database.clone() }),
            running: Mutex::new(None),
            cancel: CancelFlag::default(),
        };
        s.server_version().await.context("ClickHouse connect failed")?;
        Ok(s)
    }

    fn request(&self, sql: &str, query_id: &str) -> reqwest::RequestBuilder {
        let db = self.database.lock().unwrap().clone();
        self.http
            .post(&self.base)
            .query(&[
                ("database", db.as_str()),
                ("default_format", "JSONCompactEachRowWithNamesAndTypes"),
                ("query_id", query_id),
                ("output_format_json_quote_64bit_integers", "1"),
            ])
            .header("X-ClickHouse-User", &self.user)
            .header("X-ClickHouse-Key", &self.password)
            .body(sql.to_string())
    }
}

fn is_exception(line: &[u8]) -> bool {
    let s = String::from_utf8_lossy(line);
    s.contains("DB::Exception") && (s.contains("Code: ") || s.starts_with("[\"Code"))
}

/// `[]` `[]` `["Code: 46. DB::Exception: … (version …)"]` (one per line) → `Code: 46. DB::Exception: …`.
pub fn clean_error(body: &str) -> String {
    let text = match body.find("Code: ") {
        Some(i) => &body[i..],
        None => body.trim(),
    };
    let text = text.trim_end().trim_end_matches(']').trim_end_matches('"');
    // un-escape the JSON string the message was wrapped in
    let text = text.replace(r#"\""#, "\"").replace(r"\n", "\n").replace(r"\'", "'");
    // drop the trailing "(version …)" noise
    let text = match text.rfind(" (version ") {
        Some(i) if text.ends_with(')') => text[..i].to_string(),
        _ => text,
    };
    text.trim().to_string()
}

fn base_type(t: &str) -> &str {
    let mut t = t;
    for w in ["Nullable(", "LowCardinality("] {
        if let Some(inner) = t.strip_prefix(w) {
            t = inner.strip_suffix(')').unwrap_or(inner);
        }
    }
    t
}

fn is_numeric(t: &str) -> bool {
    let b = base_type(t);
    b.starts_with("Int") || b.starts_with("UInt") || b.starts_with("Float") || b.starts_with("Decimal")
}

fn to_cell(v: Value, t: &str) -> Cell {
    let b = base_type(t);
    match v {
        Value::Null => Cell::Null,
        Value::Bool(x) => Cell::Bool(x),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Cell::Int(i)
            } else if b.starts_with("Decimal") {
                Cell::Dec(n.to_string())
            } else {
                n.as_f64().map(Cell::Float).unwrap_or(Cell::Dec(n.to_string()))
            }
        }
        Value::String(s) => {
            if b.starts_with("Int") || b.starts_with("UInt") {
                s.parse().map(Cell::Int).unwrap_or(Cell::Dec(s))
            } else if b.starts_with("Decimal") {
                Cell::Dec(s)
            } else if b.starts_with("Date") {
                Cell::DateTime(s)
            } else {
                Cell::Text(s)
            }
        }
        other => Cell::Text(other.to_string()),
    }
}

#[async_trait]
impl Session for ChSession {
    fn dialect(&self) -> Dialect {
        Dialect::Clickhouse
    }
    fn cancel_flag(&self) -> &CancelFlag {
        &self.cancel
    }

    async fn run(&self, sql: &str, sink: &mut dyn RowSink) -> Result<RunOutcome> {
        self.cancel.reset();
        let sql = sql.trim().trim_end_matches(';');
        if let Some(db) = sql.strip_prefix("USE ").or_else(|| sql.strip_prefix("use ")) {
            *self.database.lock().unwrap() = db.trim().trim_matches('`').trim_matches('"').to_string();
            return Ok(RunOutcome { affected: Some(0) });
        }
        let qid = uuid::Uuid::new_v4().to_string();
        *self.running.lock().unwrap() = Some(qid.clone());
        let resp = self.request(sql, &qid).send().await.context("ClickHouse request failed")?;
        let status = resp.status();
        let written: Option<u64> = resp
            .headers()
            .get("X-ClickHouse-Summary")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| serde_json::from_str::<Value>(h).ok())
            .and_then(|v| v["written_rows"].as_str().and_then(|s| s.parse().ok()));
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("{}", clean_error(&body));
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = vec![];
        let mut names: Option<Vec<String>> = None;
        let mut types: Option<Vec<String>> = None;
        let mut out = RunOutcome::default();
        'outer: while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
            while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = &line[..line.len() - 1];
                if line.is_empty() {
                    continue;
                }
                // An error raised after streaming started arrives as a body line, not an HTTP status.
                if is_exception(line) {
                    bail!("{}", clean_error(&String::from_utf8_lossy(line)));
                }
                let arr: Vec<Value> = match serde_json::from_slice(line) {
                    Ok(Value::Array(a)) => a,
                    _ => {
                        // Non-JSON output (user supplied FORMAT): show raw lines.
                        if names.is_none() {
                            names = Some(vec!["result".into()]);
                            types = Some(vec!["String".into()]);
                            sink.columns(vec![ColumnMeta::new("result", "String", false)])?;
                        }
                        if !sink.row(vec![Cell::Text(String::from_utf8_lossy(line).into_owned())])? {
                            break 'outer;
                        }
                        continue;
                    }
                };
                if names.is_none() {
                    names = Some(arr.iter().map(|v| v.as_str().unwrap_or("").to_string()).collect());
                } else if types.is_none() {
                    let t: Vec<String> = arr.iter().map(|v| v.as_str().unwrap_or("").to_string()).collect();
                    sink.columns(
                        names.as_ref().unwrap().iter().zip(&t).map(|(n, t)| ColumnMeta::new(n, t, is_numeric(t))).collect(),
                    )?;
                    types = Some(t);
                } else {
                    let t = types.as_ref().unwrap();
                    let cells = arr.into_iter().zip(t).map(|(v, t)| to_cell(v, t)).collect();
                    if !sink.row(cells)? || self.cancel.is_set() {
                        break 'outer;
                    }
                }
            }
        }
        *self.running.lock().unwrap() = None;
        if names.is_none() {
            out.affected = Some(written.unwrap_or(0));
        } else if types.is_none() {
            // Names line only (zero columns typed) — still report columns.
            sink.columns(names.unwrap().into_iter().map(|n| ColumnMeta::new(n, "", false)).collect())?;
        }
        Ok(out)
    }

    async fn cancel(&self) -> Result<()> {
        self.cancel.set();
        let qid = self.running.lock().unwrap().clone();
        if let Some(q) = qid {
            let kill = format!("KILL QUERY WHERE query_id = {} SYNC", lit(&q));
            let _ = self.request(&kill, &uuid::Uuid::new_v4().to_string()).send().await;
        }
        Ok(())
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
        let r = self.request("SELECT version()", &uuid::Uuid::new_v4().to_string()).send().await?;
        if !r.status().is_success() {
            bail!("{}", r.text().await.unwrap_or_default().trim());
        }
        let t = r.text().await?;
        let v = t.lines().nth(2).and_then(|l| serde_json::from_str::<Vec<String>>(l).ok()).and_then(|v| v.into_iter().next());
        Ok(format!("ClickHouse {}", v.unwrap_or_default()))
    }

    async fn native_current_schema(&self) -> Result<Option<String>> {
        Ok(Some(self.database.lock().unwrap().clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::clean_error;

    #[test]
    fn cleans_clickhouse_errors() {
        let raw = "[]
[]
[\"Code: 46. DB::Exception: Function with name `TODATE` does not exist. Maybe you meant: ['toDate','toDate32']. (UNKNOWN_FUNCTION) (version 25.7.1.3997 (official build))\"]
";
        assert_eq!(
            clean_error(raw),
            "Code: 46. DB::Exception: Function with name `TODATE` does not exist. Maybe you meant: ['toDate','toDate32']. (UNKNOWN_FUNCTION)"
        );
        assert_eq!(clean_error("Code: 62. DB::Exception: Syntax error (version 25.8)"), "Code: 62. DB::Exception: Syntax error");
    }
}
