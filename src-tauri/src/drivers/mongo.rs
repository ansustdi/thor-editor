//! MongoDB with mongosh-style statements:
//! `db.coll.find({...}).sort({...}).limit(10)`, `db.getSiblingDB("x").getCollection("y").aggregate([...])`,
//! `db.runCommand({...})`, `show dbs`, `show collections`, `use db`.

use super::{CancelFlag, Dialect, RowSink, RunOutcome, Session};
use crate::model::{Cell, ColumnInfo, ColumnMeta, ConnectionProfile, DbObject};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use bson::{doc, Bson, Document};
use futures::TryStreamExt;
use mongodb::Client;
use regex::Regex;
use std::sync::{Mutex, OnceLock};

pub struct MongoSession {
    client: Client,
    db: Mutex<String>,
    cancel: CancelFlag,
}

#[derive(Debug, Default, PartialEq)]
pub struct Parsed {
    pub db: Option<String>,
    pub coll: Option<String>,
    pub calls: Vec<(String, Vec<serde_json::Value>)>,
}

/// Rewrites mongosh helpers (ObjectId, ISODate, NumberLong…) into Extended JSON.
fn preprocess(args: &str) -> String {
    static RES: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let res = RES.get_or_init(|| {
        vec![
            (Regex::new(r#"ObjectId\(\s*["']([0-9a-fA-F]{24})["']\s*\)"#).unwrap(), r#"{"$$oid":"$1"}"#),
            (Regex::new(r#"(?:new\s+)?(?:ISODate|Date)\(\s*["']([^"']*)["']\s*\)"#).unwrap(), r#"{"$$date":"$1"}"#),
            (Regex::new(r#"NumberLong\(\s*["']?(-?\d+)["']?\s*\)"#).unwrap(), r#"{"$$numberLong":"$1"}"#),
            (Regex::new(r#"NumberInt\(\s*["']?(-?\d+)["']?\s*\)"#).unwrap(), "$1"),
            (Regex::new(r#"NumberDecimal\(\s*["']([^"']*)["']\s*\)"#).unwrap(), r#"{"$$numberDecimal":"$1"}"#),
            (Regex::new(r#"UUID\(\s*["']([0-9a-fA-F-]{36})["']\s*\)"#).unwrap(), r#"{"$$uuid":"$1"}"#),
        ]
    });
    let mut s = args.to_string();
    for (re, rep) in res {
        s = re.replace_all(&s, *rep).into_owned();
    }
    s
}

fn parse_args(raw: &str) -> Result<Vec<serde_json::Value>> {
    if raw.trim().is_empty() {
        return Ok(vec![]);
    }
    let v: serde_json::Value =
        json5::from_str(&format!("[{}]", preprocess(raw))).map_err(|e| anyhow!("cannot parse arguments: {e}"))?;
    Ok(v.as_array().cloned().unwrap_or_default())
}

/// Returns the text inside the balanced parentheses starting at `open` and the index after `)`.
fn balanced(s: &[char], open: usize) -> Result<(String, usize)> {
    let mut depth = 0;
    let mut i = open;
    let mut quote: Option<char> = None;
    while i < s.len() {
        let c = s[i];
        if let Some(q) = quote {
            if c == '\\' {
                i += 1;
            } else if c == q {
                quote = None;
            }
        } else {
            match c {
                '"' | '\'' | '`' => quote = Some(c),
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok((s[open + 1..i].iter().collect(), i + 1));
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    bail!("unbalanced brackets")
}

pub fn parse(stmt: &str) -> Result<Parsed> {
    let s: Vec<char> = stmt.trim().trim_end_matches(';').chars().collect();
    let text: String = s.iter().collect();
    let low = text.to_ascii_lowercase();
    if let Some(rest) = low.strip_prefix("show ") {
        let what = rest.trim();
        let m = match what {
            "dbs" | "databases" => "$showDbs",
            "collections" | "tables" => "$showCollections",
            _ => bail!("unsupported: show {what}"),
        };
        return Ok(Parsed { calls: vec![(m.into(), vec![])], ..Default::default() });
    }
    if low.starts_with("use ") {
        return Ok(Parsed { db: Some(text[4..].trim().to_string()), calls: vec![("$use".into(), vec![])], ..Default::default() });
    }
    if !low.starts_with("db") {
        bail!("statements must start with `db.`, `show` or `use` (e.g. db.users.find({{}}).limit(10))");
    }
    let mut p = Parsed::default();
    let mut props: Vec<String> = vec![];
    let mut i = 2;
    let ident = |i: &mut usize| -> String {
        let start = *i;
        while *i < s.len() && (s[*i].is_alphanumeric() || s[*i] == '_' || s[*i] == '$' || s[*i] == '-') {
            *i += 1;
        }
        s[start..*i].iter().collect()
    };
    while i < s.len() {
        let c = s[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '.' {
            i += 1;
            while i < s.len() && s[i].is_whitespace() {
                i += 1;
            }
            let name = ident(&mut i);
            let mut j = i;
            while j < s.len() && s[j].is_whitespace() {
                j += 1;
            }
            if j < s.len() && s[j] == '(' {
                let (inner, next) = balanced(&s, j)?;
                i = next;
                match name.as_str() {
                    "getSiblingDB" => {
                        p.db = parse_args(&inner)?.first().and_then(|v| v.as_str()).map(String::from);
                        props.clear();
                    }
                    "getCollection" => p.coll = parse_args(&inner)?.first().and_then(|v| v.as_str()).map(String::from),
                    _ => {
                        if p.coll.is_none() && !props.is_empty() {
                            p.coll = Some(props.join("."));
                            props.clear();
                        }
                        p.calls.push((name, parse_args(&inner)?));
                    }
                }
            } else if p.calls.is_empty() {
                props.push(name);
            } else {
                bail!("unexpected property `.{name}` after a method call");
            }
        } else if c == '[' {
            let (inner, next) = balanced(&s, i)?;
            i = next;
            p.coll = parse_args(&inner)?.first().and_then(|v| v.as_str()).map(String::from);
        } else {
            bail!("unexpected `{c}` at position {i}");
        }
    }
    if p.calls.is_empty() {
        bail!("no method call found (e.g. .find())");
    }
    Ok(p)
}

fn to_doc(v: Option<&serde_json::Value>) -> Result<Document> {
    match v {
        None => Ok(Document::new()),
        Some(v) => match Bson::try_from(v.clone()).context("invalid extended JSON")? {
            Bson::Document(d) => Ok(d),
            other => bail!("expected a document, got {other}"),
        },
    }
}

fn to_docs(v: Option<&serde_json::Value>) -> Result<Vec<Document>> {
    match v {
        Some(serde_json::Value::Array(a)) => a.iter().map(|x| to_doc(Some(x))).collect(),
        Some(_) => bail!("expected an array of documents"),
        None => Ok(vec![]),
    }
}

pub fn bson_cell(b: &Bson) -> Cell {
    match b {
        Bson::Null | Bson::Undefined => Cell::Null,
        Bson::Boolean(v) => Cell::Bool(*v),
        Bson::Int32(v) => Cell::Int(*v as i64),
        Bson::Int64(v) => Cell::Int(*v),
        Bson::Double(v) => Cell::Float(*v),
        Bson::Decimal128(d) => Cell::Dec(d.to_string()),
        Bson::String(s) => Cell::Text(s.clone()),
        Bson::ObjectId(o) => Cell::Text(o.to_hex()),
        Bson::DateTime(d) => Cell::DateTime(d.try_to_rfc3339_string().unwrap_or_else(|_| d.to_string())),
        Bson::Binary(b) => Cell::Bytes(b.bytes.clone()),
        other => Cell::Text(other.clone().into_relaxed_extjson().to_string()),
    }
}

/// Buffers the first documents to infer columns, then streams.
struct DocSink<'a> {
    sink: &'a mut dyn RowSink,
    buffer: Vec<Document>,
    cols: Option<Vec<String>>,
    stopped: bool,
}

const INFER: usize = 200;

impl<'a> DocSink<'a> {
    fn new(sink: &'a mut dyn RowSink) -> Self {
        Self { sink, buffer: vec![], cols: None, stopped: false }
    }
    fn header(&mut self) -> Result<()> {
        let mut cols: Vec<String> = vec![];
        for d in &self.buffer {
            for k in d.keys() {
                if !cols.contains(k) {
                    cols.push(k.clone());
                }
            }
        }
        if let Some(i) = cols.iter().position(|c| c == "_id") {
            let id = cols.remove(i);
            cols.insert(0, id);
        }
        let numeric = |c: &String| {
            self.buffer
                .iter()
                .find_map(|d| d.get(c))
                .map(|b| matches!(b, Bson::Int32(_) | Bson::Int64(_) | Bson::Double(_) | Bson::Decimal128(_)))
                .unwrap_or(false)
        };
        self.sink.columns(cols.iter().map(|c| ColumnMeta::new(c, "", numeric(c))).collect())?;
        self.cols = Some(cols);
        for d in std::mem::take(&mut self.buffer) {
            if !self.emit(&d)? {
                self.stopped = true;
                break;
            }
        }
        Ok(())
    }
    fn emit(&mut self, d: &Document) -> Result<bool> {
        let cols = self.cols.as_ref().unwrap();
        let row = cols.iter().map(|c| d.get(c).map(bson_cell).unwrap_or(Cell::Null)).collect();
        self.sink.row(row)
    }
    fn push(&mut self, d: Document) -> Result<bool> {
        if self.stopped {
            return Ok(false);
        }
        if self.cols.is_none() {
            self.buffer.push(d);
            if self.buffer.len() >= INFER {
                self.header()?;
            }
            return Ok(!self.stopped);
        }
        self.emit(&d)
    }
    fn finish(mut self) -> Result<()> {
        if self.cols.is_none() {
            self.header()?;
        }
        Ok(())
    }
}

fn single(sink: &mut dyn RowSink, name: &str, v: Cell) -> Result<()> {
    sink.columns(vec![ColumnMeta::new(name, "", v.is_numeric_like())])?;
    sink.row(vec![v])?;
    Ok(())
}

trait NumericLike {
    fn is_numeric_like(&self) -> bool;
}
impl NumericLike for Cell {
    fn is_numeric_like(&self) -> bool {
        matches!(self, Cell::Int(_) | Cell::Float(_))
    }
}

impl MongoSession {
    pub async fn connect(p: &ConnectionProfile, password: &str) -> Result<Self> {
        let uri = match p.params.get("uri").filter(|u| !u.is_empty()) {
            Some(u) => u.clone(),
            None => {
                let host = if p.host.is_empty() { "localhost" } else { &p.host };
                let mut u = url::Url::parse(&format!("mongodb://{host}:{}/", p.port_or_default()))?;
                if !p.username.is_empty() {
                    u.set_username(&p.username).ok();
                    u.set_password(Some(password)).ok();
                }
                let auth_src = p.params.get("authSource").cloned().unwrap_or_else(|| "admin".into());
                {
                    let mut q = u.query_pairs_mut();
                    if !p.username.is_empty() {
                        q.append_pair("authSource", &auth_src);
                    }
                    if matches!(p.ssl_mode.as_str(), "require" | "verify-full") {
                        q.append_pair("tls", "true");
                    }
                    if p.ssl_mode == "require" {
                        q.append_pair("tlsAllowInvalidCertificates", "true");
                    }
                    q.append_pair("serverSelectionTimeoutMS", "15000");
                    q.append_pair("appName", "Thor Editor");
                }
                u.to_string()
            }
        };
        let client = Client::with_uri_str(&uri).await.context("MongoDB connect failed")?;
        let db = if p.database.is_empty() {
            client.default_database().map(|d| d.name().to_string()).unwrap_or_else(|| "test".into())
        } else {
            p.database.clone()
        };
        client.database("admin").run_command(doc! { "ping": 1 }).await.context("MongoDB ping failed")?;
        Ok(Self { client, db: Mutex::new(db), cancel: CancelFlag::default() })
    }

    fn current_db(&self) -> String {
        self.db.lock().unwrap().clone()
    }

    async fn stream_cursor(
        &self,
        mut cursor: mongodb::Cursor<Document>,
        sink: &mut dyn RowSink,
    ) -> Result<()> {
        let mut ds = DocSink::new(sink);
        while let Some(d) = cursor.try_next().await? {
            if !ds.push(d)? || self.cancel.is_set() {
                break;
            }
        }
        ds.finish()
    }
}

fn arg<'a>(args: &'a [serde_json::Value], i: usize) -> Option<&'a serde_json::Value> {
    args.get(i)
}

#[async_trait]
impl Session for MongoSession {
    fn dialect(&self) -> Dialect {
        Dialect::Mongo
    }
    fn cancel_flag(&self) -> &CancelFlag {
        &self.cancel
    }

    async fn run(&self, stmt: &str, sink: &mut dyn RowSink) -> Result<RunOutcome> {
        self.cancel.reset();
        let p = parse(stmt)?;
        let dbname = p.db.clone().unwrap_or_else(|| self.current_db());
        let db = self.client.database(&dbname);
        let (method, args) = p.calls[0].clone();
        let mods = &p.calls[1..];
        let affected = |n: u64| Ok(RunOutcome { affected: Some(n) });

        let Some(coll_name) = p.coll.clone() else {
            // database-level operations
            return match method.as_str() {
                "$use" => {
                    *self.db.lock().unwrap() = dbname;
                    affected(0)
                }
                "$showDbs" => {
                    let mut ds = DocSink::new(sink);
                    for d in self.client.list_databases().await? {
                        ds.push(doc! { "name": d.name, "sizeOnDisk": d.size_on_disk as i64, "empty": d.empty })?;
                    }
                    ds.finish()?;
                    Ok(RunOutcome::default())
                }
                "$showCollections" | "getCollectionNames" => {
                    let mut names = db.list_collection_names().await?;
                    names.sort();
                    sink.columns(vec![ColumnMeta::new("collection", "", false)])?;
                    for n in names {
                        sink.row(vec![Cell::Text(n)])?;
                    }
                    Ok(RunOutcome::default())
                }
                "runCommand" | "adminCommand" => {
                    let target = if method == "adminCommand" { self.client.database("admin") } else { db };
                    let res = target.run_command(to_doc(arg(&args, 0))?).await?;
                    let mut ds = DocSink::new(sink);
                    ds.push(res)?;
                    ds.finish()?;
                    Ok(RunOutcome::default())
                }
                "stats" => {
                    let res = db.run_command(doc! { "dbStats": 1 }).await?;
                    let mut ds = DocSink::new(sink);
                    ds.push(res)?;
                    ds.finish()?;
                    Ok(RunOutcome::default())
                }
                "createCollection" => {
                    let name = arg(&args, 0).and_then(|v| v.as_str()).ok_or_else(|| anyhow!("collection name required"))?;
                    db.create_collection(name).await?;
                    affected(0)
                }
                "dropDatabase" => {
                    db.drop().await?;
                    affected(0)
                }
                m => bail!("unsupported database method `{m}`"),
            };
        };

        let coll = db.collection::<Document>(&coll_name);
        match method.as_str() {
            "find" | "findOne" => {
                let mut f = coll.find(to_doc(arg(&args, 0))?);
                if let Some(proj) = arg(&args, 1) {
                    f = f.projection(to_doc(Some(proj))?);
                }
                if method == "findOne" {
                    f = f.limit(1);
                }
                let mut want_count = false;
                for (m, a) in mods {
                    match m.as_str() {
                        "sort" => f = f.sort(to_doc(a.first())?),
                        "limit" => f = f.limit(a.first().and_then(|v| v.as_i64()).unwrap_or(0)),
                        "skip" => f = f.skip(a.first().and_then(|v| v.as_u64()).unwrap_or(0)),
                        "projection" | "project" => f = f.projection(to_doc(a.first())?),
                        "hint" => f = f.hint(mongodb::options::Hint::Keys(to_doc(a.first())?)),
                        "count" | "itcount" => want_count = true,
                        "toArray" | "pretty" => {}
                        other => bail!("unsupported cursor method `{other}`"),
                    }
                }
                if want_count {
                    let n = coll.count_documents(to_doc(arg(&args, 0))?).await?;
                    single(sink, "count", Cell::Int(n as i64))?;
                    return Ok(RunOutcome::default());
                }
                let cursor = f.await?;
                self.stream_cursor(cursor, sink).await?;
                Ok(RunOutcome::default())
            }
            "aggregate" => {
                let cursor = coll.aggregate(to_docs(arg(&args, 0))?).await?;
                self.stream_cursor(cursor, sink).await?;
                Ok(RunOutcome::default())
            }
            "countDocuments" | "count" => {
                let n = coll.count_documents(to_doc(arg(&args, 0))?).await?;
                single(sink, "count", Cell::Int(n as i64))?;
                Ok(RunOutcome::default())
            }
            "estimatedDocumentCount" => {
                let n = coll.estimated_document_count().await?;
                single(sink, "count", Cell::Int(n as i64))?;
                Ok(RunOutcome::default())
            }
            "distinct" => {
                let field = arg(&args, 0).and_then(|v| v.as_str()).ok_or_else(|| anyhow!("field name required"))?;
                let vals = coll.distinct(field, to_doc(arg(&args, 1))?).await?;
                sink.columns(vec![ColumnMeta::new(field, "", false)])?;
                for v in vals {
                    if !sink.row(vec![bson_cell(&v)])? {
                        break;
                    }
                }
                Ok(RunOutcome::default())
            }
            "insertOne" => {
                coll.insert_one(to_doc(arg(&args, 0))?).await?;
                affected(1)
            }
            "insertMany" => {
                let r = coll.insert_many(to_docs(arg(&args, 0))?).await?;
                affected(r.inserted_ids.len() as u64)
            }
            "updateOne" | "updateMany" => {
                let (f, u) = (to_doc(arg(&args, 0))?, arg(&args, 1).ok_or_else(|| anyhow!("update document required"))?);
                let upd: mongodb::options::UpdateModifications = match u {
                    serde_json::Value::Array(_) => to_docs(Some(u))?.into(),
                    _ => to_doc(Some(u))?.into(),
                };
                let r = if method == "updateOne" { coll.update_one(f, upd).await? } else { coll.update_many(f, upd).await? };
                affected(r.modified_count)
            }
            "replaceOne" => {
                let r = coll.replace_one(to_doc(arg(&args, 0))?, to_doc(arg(&args, 1))?).await?;
                affected(r.modified_count)
            }
            "deleteOne" => affected(coll.delete_one(to_doc(arg(&args, 0))?).await?.deleted_count),
            "deleteMany" | "remove" => affected(coll.delete_many(to_doc(arg(&args, 0))?).await?.deleted_count),
            "drop" => {
                coll.drop().await?;
                affected(0)
            }
            "createIndex" => {
                let mut model = mongodb::IndexModel::builder().keys(to_doc(arg(&args, 0))?).build();
                if let Some(o) = arg(&args, 1) {
                    let opts: mongodb::options::IndexOptions = bson::from_document(to_doc(Some(o))?)?;
                    model.options = Some(opts);
                }
                let r = coll.create_index(model).await?;
                single(sink, "index", Cell::Text(r.index_name))?;
                Ok(RunOutcome::default())
            }
            "dropIndex" => {
                let name = arg(&args, 0).and_then(|v| v.as_str()).ok_or_else(|| anyhow!("index name required"))?;
                coll.drop_index(name).await?;
                affected(0)
            }
            "getIndexes" => {
                let cursor = coll.list_indexes().await?;
                let idx: Vec<mongodb::IndexModel> = cursor.try_collect().await?;
                let mut ds = DocSink::new(sink);
                for m in idx {
                    let mut d = doc! { "key": m.keys };
                    if let Some(o) = m.options {
                        d.extend(bson::to_document(&o)?);
                    }
                    ds.push(d)?;
                }
                ds.finish()?;
                Ok(RunOutcome::default())
            }
            m => bail!("unsupported collection method `{m}`"),
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
        let r = self.client.database("admin").run_command(doc! { "buildInfo": 1 }).await?;
        Ok(format!("MongoDB {}", r.get_str("version").unwrap_or("?")))
    }

    async fn native_current_schema(&self) -> Result<Option<String>> {
        Ok(Some(self.current_db()))
    }

    async fn native_schemas(&self) -> Result<Option<Vec<String>>> {
        let mut names = self.client.list_database_names().await?;
        let cur = self.current_db();
        if !names.contains(&cur) {
            names.push(cur);
        }
        names.sort();
        Ok(Some(names))
    }

    async fn native_objects(&self, schema: &str) -> Result<Option<Vec<DbObject>>> {
        let db = self.client.database(schema);
        let specs: Vec<mongodb::results::CollectionSpecification> = db.list_collections().await?.try_collect().await?;
        let mut out: Vec<DbObject> = specs
            .into_iter()
            .map(|s| DbObject {
                schema: schema.to_string(),
                kind: match s.collection_type {
                    mongodb::results::CollectionType::View => "view".into(),
                    mongodb::results::CollectionType::Timeseries => "timeseries".into(),
                    _ => "collection".into(),
                },
                name: s.name,
            })
            .collect();
        out.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.name.cmp(&b.name)));
        Ok(Some(out))
    }

    async fn native_columns(&self, schema: &str, name: &str) -> Result<Option<Vec<ColumnInfo>>> {
        let coll = self.client.database(schema).collection::<Document>(name);
        let docs: Vec<Document> = coll.find(doc! {}).limit(100).await?.try_collect().await?;
        let mut cols: Vec<ColumnInfo> = vec![];
        for d in &docs {
            for (k, v) in d {
                if let Some(c) = cols.iter_mut().find(|c| &c.name == k) {
                    if !c.data_type.split(" | ").any(|t| t == type_name(v)) {
                        c.data_type = format!("{} | {}", c.data_type, type_name(v));
                    }
                } else {
                    cols.push(ColumnInfo {
                        name: k.clone(),
                        data_type: type_name(v).into(),
                        nullable: true,
                        default_value: None,
                        primary_key: k == "_id",
                        comment: None,
                    });
                }
            }
        }
        for c in &mut cols {
            c.nullable = docs.iter().any(|d| !d.contains_key(&c.name) || matches!(d.get(&c.name), Some(Bson::Null)));
        }
        Ok(Some(cols))
    }

    async fn native_ddl(&self, schema: &str, name: &str, _kind: &str) -> Result<Option<String>> {
        let coll = self.client.database(schema).collection::<Document>(name);
        let idx: Vec<mongodb::IndexModel> = coll.list_indexes().await?.try_collect().await?;
        let mut out = format!("db.getSiblingDB({:?}).createCollection({:?});\n", schema, name);
        for m in idx {
            let keys = Bson::Document(m.keys).into_relaxed_extjson();
            let opts = m.options.map(|o| bson::to_document(&o).map(|d| Bson::Document(d).into_relaxed_extjson()));
            match opts {
                Some(Ok(o)) => out.push_str(&format!("db.getSiblingDB({schema:?}).getCollection({name:?}).createIndex({keys}, {o});\n")),
                _ => out.push_str(&format!("db.getSiblingDB({schema:?}).getCollection({name:?}).createIndex({keys});\n")),
            }
        }
        Ok(Some(out))
    }
}

fn type_name(b: &Bson) -> &'static str {
    match b {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Array(_) => "array",
        Bson::Document(_) => "object",
        Bson::Boolean(_) => "bool",
        Bson::Null => "null",
        Bson::Int32(_) => "int",
        Bson::Int64(_) => "long",
        Bson::Decimal128(_) => "decimal",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "date",
        Bson::Binary(_) => "binData",
        Bson::Timestamp(_) => "timestamp",
        Bson::RegularExpression(_) => "regex",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chain() {
        let p = parse(r#"db.users.find({age: {$gt: 30}, _id: ObjectId("5f1d7f9b2c3a4b5c6d7e8f90")}, {name: 1}).sort({age: -1}).limit(5);"#).unwrap();
        assert_eq!(p.coll.as_deref(), Some("users"));
        assert_eq!(p.calls.len(), 3);
        assert_eq!(p.calls[0].1[0]["age"]["$gt"], 30);
        assert_eq!(p.calls[0].1[0]["_id"]["$oid"], "5f1d7f9b2c3a4b5c6d7e8f90");
        assert_eq!(p.calls[2].1[0], 5);
    }

    #[test]
    fn parses_sibling_and_brackets() {
        let p = parse(r#"db.getSiblingDB("shop").getCollection("order.items").aggregate([{ $match: { s: 'a)b' } }])"#).unwrap();
        assert_eq!(p.db.as_deref(), Some("shop"));
        assert_eq!(p.coll.as_deref(), Some("order.items"));
        assert_eq!(p.calls[0].1[0][0]["$match"]["s"], "a)b");
        let p = parse(r#"db["my-coll"].countDocuments()"#).unwrap();
        assert_eq!(p.coll.as_deref(), Some("my-coll"));
        let p = parse("db.system.users.find()").unwrap();
        assert_eq!(p.coll.as_deref(), Some("system.users"));
        let p = parse("db.runCommand({ping: 1})").unwrap();
        assert_eq!(p.coll, None);
        assert!(parse("show dbs").is_ok());
        assert!(parse("select 1").is_err());
    }
}
