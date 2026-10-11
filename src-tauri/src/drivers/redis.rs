//! Redis: redis-cli style commands, results shaped into grids per command.

use super::{CancelFlag, Dialect, RowSink, RunOutcome, Session};
use crate::model::{Cell, ColumnInfo, ColumnMeta, ConnectionProfile, DbObject};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use redis::aio::MultiplexedConnection;
use redis::Value;
use std::sync::atomic::{AtomicI64, Ordering};

pub struct RedisSession {
    conn: MultiplexedConnection,
    db: AtomicI64,
    cancel: CancelFlag,
}

const MAX_KEYS: usize = 10_000;

impl RedisSession {
    pub async fn connect(p: &ConnectionProfile, password: &str) -> Result<Self> {
        let host = if p.host.is_empty() { "localhost" } else { &p.host };
        let db: i64 = p.database.trim().trim_start_matches("db").parse().unwrap_or(0);
        let tls = matches!(p.ssl_mode.as_str(), "require" | "verify-full");
        let mut u = url::Url::parse(&format!("{}://{host}:{}/{db}", if tls { "rediss" } else { "redis" }, p.port_or_default()))?;
        if !p.username.is_empty() {
            u.set_username(&p.username).ok();
        }
        if !password.is_empty() {
            u.set_password(Some(password)).ok();
        }
        if p.ssl_mode == "require" {
            u.set_fragment(Some("insecure"));
        }
        let client = redis::Client::open(u.as_str())?;
        let conn = tokio::time::timeout(std::time::Duration::from_secs(15), client.get_multiplexed_async_connection())
            .await
            .context("Redis connect timed out")?
            .context("Redis connect failed")?;
        Ok(Self { conn, db: AtomicI64::new(db), cancel: CancelFlag::default() })
    }

    async fn call(&self, args: &[String]) -> Result<Value> {
        let mut c = redis::cmd(&args[0]);
        for a in &args[1..] {
            c.arg(a.as_bytes());
        }
        let mut conn = self.conn.clone();
        Ok(c.query_async::<Value>(&mut conn).await?)
    }
}

/// redis-cli style tokenizer: whitespace separated, "double" (with escapes) and 'single' quotes.
pub fn tokenize(line: &str) -> Result<Vec<String>> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut in_tok = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_tok = true;
                loop {
                    match chars.next() {
                        None => bail!("unterminated \" quote"),
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => cur.push('\n'),
                            Some('r') => cur.push('\r'),
                            Some('t') => cur.push('\t'),
                            Some('x') => {
                                let h: String = [chars.next(), chars.next()].iter().flatten().collect();
                                cur.push(u8::from_str_radix(&h, 16).map(|b| b as char).unwrap_or('?'));
                            }
                            Some(o) => cur.push(o),
                            None => bail!("dangling escape"),
                        },
                        Some(o) => cur.push(o),
                    }
                }
            }
            '\'' => {
                in_tok = true;
                loop {
                    match chars.next() {
                        None => bail!("unterminated ' quote"),
                        Some('\'') => break,
                        Some(o) => cur.push(o),
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_tok {
                    out.push(std::mem::take(&mut cur));
                    in_tok = false;
                }
            }
            c => {
                in_tok = true;
                cur.push(c);
            }
        }
    }
    if in_tok {
        out.push(cur);
    }
    Ok(out)
}

fn cell(v: &Value) -> Cell {
    match v {
        Value::Nil => Cell::Null,
        Value::Int(i) => Cell::Int(*i),
        Value::Double(f) => Cell::Float(*f),
        Value::Boolean(b) => Cell::Bool(*b),
        Value::Okay => Cell::Text("OK".into()),
        Value::SimpleString(s) => Cell::Text(s.clone()),
        Value::BulkString(b) => match std::str::from_utf8(b) {
            Ok(s) => Cell::Text(s.to_string()),
            Err(_) => Cell::Bytes(b.clone()),
        },
        Value::VerbatimString { text, .. } => Cell::Text(text.clone()),
        other => Cell::Text(json(other).to_string()),
    }
}

fn json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Nil => J::Null,
        Value::Int(i) => J::from(*i),
        Value::Double(f) => J::from(*f),
        Value::Boolean(b) => J::from(*b),
        Value::Array(a) | Value::Set(a) => J::Array(a.iter().map(json).collect()),
        Value::Map(m) => J::Object(m.iter().map(|(k, v)| (cell(k).to_text(), json(v))).collect()),
        other => J::String(cell(other).to_text()),
    }
}

fn items(v: &Value) -> Option<&Vec<Value>> {
    match v {
        Value::Array(a) | Value::Set(a) => Some(a),
        _ => None,
    }
}

fn emit_pairs(sink: &mut dyn RowSink, a: &str, b: &str, flat: &[Value]) -> Result<()> {
    sink.columns(vec![ColumnMeta::new(a, "", false), ColumnMeta::new(b, "", false)])?;
    for pair in flat.chunks(2) {
        let r = vec![cell(&pair[0]), pair.get(1).map(cell).unwrap_or(Cell::Null)];
        if !sink.row(r)? {
            break;
        }
    }
    Ok(())
}

fn shape(cmd: &str, args: &[String], v: &Value, sink: &mut dyn RowSink) -> Result<()> {
    let with_scores = args.iter().any(|a| a.eq_ignore_ascii_case("WITHSCORES"));
    match (cmd, v) {
        (_, Value::Map(m)) => {
            sink.columns(vec![ColumnMeta::new("field", "", false), ColumnMeta::new("value", "", false)])?;
            for (k, v) in m {
                if !sink.row(vec![cell(k), cell(v)])? {
                    break;
                }
            }
        }
        ("HGETALL" | "CONFIG", v) if items(v).is_some() => emit_pairs(sink, "field", "value", items(v).unwrap())?,
        (c, v) if c.starts_with('Z') && with_scores && items(v).is_some() => {
            emit_pairs(sink, "member", "score", items(v).unwrap())?
        }
        ("SCAN" | "SSCAN" | "HSCAN" | "ZSCAN", Value::Array(a)) if a.len() == 2 => {
            let next = cell(&a[0]).to_text();
            let list = items(&a[1]).cloned().unwrap_or_default();
            match cmd {
                "HSCAN" => emit_pairs(sink, &format!("field (next cursor {next})"), "value", &list)?,
                "ZSCAN" => emit_pairs(sink, &format!("member (next cursor {next})"), "score", &list)?,
                _ => {
                    sink.columns(vec![ColumnMeta::new(format!("value (next cursor {next})"), "", false)])?;
                    for x in &list {
                        if !sink.row(vec![cell(x)])? {
                            break;
                        }
                    }
                }
            }
        }
        ("XRANGE" | "XREVRANGE", Value::Array(a)) => {
            sink.columns(vec![ColumnMeta::new("id", "", false), ColumnMeta::new("fields", "json", false)])?;
            for e in a {
                if let Some(parts) = items(e) {
                    let id = parts.first().map(cell).unwrap_or(Cell::Null);
                    let fields = parts.get(1).and_then(items).map(|f| {
                        serde_json::Value::Object(
                            f.chunks(2).map(|p| (cell(&p[0]).to_text(), p.get(1).map(json).unwrap_or_default())).collect(),
                        )
                    });
                    if !sink.row(vec![id, fields.map(|f| Cell::Text(f.to_string())).unwrap_or(Cell::Null)])? {
                        break;
                    }
                }
            }
        }
        ("INFO", v) => {
            let text = cell(v).to_text();
            sink.columns(vec![
                ColumnMeta::new("section", "", false),
                ColumnMeta::new("key", "", false),
                ColumnMeta::new("value", "", false),
            ])?;
            let mut section = String::new();
            for line in text.lines() {
                let line = line.trim();
                if let Some(s) = line.strip_prefix("# ") {
                    section = s.to_string();
                } else if let Some((k, v)) = line.split_once(':') {
                    sink.row(vec![Cell::Text(section.clone()), Cell::Text(k.into()), Cell::Text(v.into())])?;
                }
            }
        }
        (_, Value::Array(a)) | (_, Value::Set(a)) => {
            sink.columns(vec![ColumnMeta::new("#", "int", true), ColumnMeta::new("value", "", false)])?;
            for (i, x) in a.iter().enumerate() {
                if !sink.row(vec![Cell::Int(i as i64 + 1), cell(x)])? {
                    break;
                }
            }
        }
        (_, v) => {
            sink.columns(vec![ColumnMeta::new("value", "", false)])?;
            sink.row(vec![cell(v)])?;
        }
    }
    Ok(())
}

#[async_trait]
impl Session for RedisSession {
    fn dialect(&self) -> Dialect {
        Dialect::Redis
    }
    fn cancel_flag(&self) -> &CancelFlag {
        &self.cancel
    }

    async fn run(&self, line: &str, sink: &mut dyn RowSink) -> Result<RunOutcome> {
        self.cancel.reset();
        let args = tokenize(line.trim().trim_end_matches(';'))?;
        if args.is_empty() {
            return Ok(RunOutcome { affected: Some(0) });
        }
        let cmd = args[0].to_ascii_uppercase();
        let v = self.call(&args).await?;
        if let Value::ServerError(e) = &v {
            bail!("{e:?}");
        }
        if cmd == "SELECT" {
            if let Ok(n) = args.get(1).map(|s| s.parse::<i64>()).transpose() {
                self.db.store(n.unwrap_or(0), Ordering::SeqCst);
            }
        }
        shape(&cmd, &args, &v, sink)?;
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
        let info = cell(&self.call(&["INFO".into(), "server".into()]).await?).to_text();
        let v = info.lines().find_map(|l| l.strip_prefix("redis_version:")).unwrap_or("?").trim().to_string();
        Ok(format!("Redis {v}"))
    }

    async fn native_current_schema(&self) -> Result<Option<String>> {
        Ok(Some(format!("db{}", self.db.load(Ordering::SeqCst))))
    }

    async fn native_schemas(&self) -> Result<Option<Vec<String>>> {
        let info = cell(&self.call(&["INFO".into(), "keyspace".into()]).await?).to_text();
        let mut dbs: Vec<i64> = info
            .lines()
            .filter_map(|l| l.strip_prefix("db")?.split(':').next()?.parse().ok())
            .collect();
        let cur = self.db.load(Ordering::SeqCst);
        if !dbs.contains(&cur) {
            dbs.push(cur);
        }
        dbs.sort();
        Ok(Some(dbs.into_iter().map(|d| format!("db{d}")).collect()))
    }

    /// Browsing a database switches the session to it (like redis-cli `SELECT`).
    async fn native_objects(&self, schema: &str) -> Result<Option<Vec<DbObject>>> {
        let n: i64 = schema.trim_start_matches("db").parse().unwrap_or(0);
        self.call(&["SELECT".into(), n.to_string()]).await?;
        self.db.store(n, Ordering::SeqCst);
        let mut keys: Vec<String> = vec![];
        let mut cursor = "0".to_string();
        loop {
            let v = self.call(&["SCAN".into(), cursor.clone(), "COUNT".into(), "1000".into()]).await?;
            let Value::Array(a) = v else { break };
            cursor = a.first().map(|c| cell(c).to_text()).unwrap_or_else(|| "0".into());
            if let Some(list) = a.get(1).and_then(items) {
                keys.extend(list.iter().map(|k| cell(k).to_text()));
            }
            if cursor == "0" || keys.len() >= MAX_KEYS {
                break;
            }
        }
        keys.sort();
        keys.truncate(MAX_KEYS);
        let mut objs = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(500) {
            let mut pipe = redis::pipe();
            for k in chunk {
                pipe.cmd("TYPE").arg(k);
            }
            let mut conn = self.conn.clone();
            let types: Vec<String> = pipe.query_async(&mut conn).await?;
            for (k, t) in chunk.iter().zip(types) {
                objs.push(DbObject { schema: schema.to_string(), name: k.clone(), kind: t });
            }
        }
        Ok(Some(objs))
    }

    async fn native_columns(&self, _schema: &str, _name: &str) -> Result<Option<Vec<ColumnInfo>>> {
        Ok(Some(vec![]))
    }

    async fn native_ddl(&self, _schema: &str, name: &str, kind: &str) -> Result<Option<String>> {
        let k = name.to_string();
        let ttl = cell(&self.call(&["TTL".into(), k.clone()]).await?).to_text();
        let enc = self.call(&["OBJECT".into(), "ENCODING".into(), k.clone()]).await.map(|v| cell(&v).to_text()).unwrap_or_default();
        let mem = self.call(&["MEMORY".into(), "USAGE".into(), k.clone()]).await.map(|v| cell(&v).to_text()).unwrap_or_default();
        let len_cmd = match kind {
            "hash" => "HLEN",
            "list" => "LLEN",
            "set" => "SCARD",
            "zset" => "ZCARD",
            "stream" => "XLEN",
            _ => "STRLEN",
        };
        let len = self.call(&[len_cmd.into(), k.clone()]).await.map(|v| cell(&v).to_text()).unwrap_or_default();
        Ok(Some(format!(
            "# key: {name}\n# type: {kind}\n# length ({len_cmd}): {len}\n# ttl: {}\n# encoding: {enc}\n# memory: {mem} bytes",
            if ttl == "-1" { "none".to_string() } else { format!("{ttl} s") }
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::tokenize;

    #[test]
    fn tokenizes_quotes() {
        assert_eq!(tokenize(r#"SET "my key" 'a b' x\y"#).unwrap(), vec!["SET", "my key", "a b", "x\\y"]);
        assert_eq!(tokenize(r#"SET k "a\"b\n""#).unwrap(), vec!["SET", "k", "a\"b\n"]);
    }
}
