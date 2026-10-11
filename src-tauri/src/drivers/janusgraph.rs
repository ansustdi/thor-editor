//! JanusGraph (or any TinkerPop Gremlin Server) over WebSocket with GraphSON 3.
//! Gremlin scripts are evaluated sessionless (the server commits each request) with `g`
//! aliased to the selected traversal source. Vertices/edges/paths become JSON cells with the
//! same `~id`/`~labels`/`~type`/`~start`/`~end`/`~path` markers as Neo4j so the UI can draw a graph.

use super::{CancelFlag, Dialect, RowSink, RunOutcome, Session};
use crate::model::{Cell, ColumnInfo, ColumnMeta, ConnectionProfile, DbObject};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const MIME: &str = "application/vnd.gremlin-v3.0+json";

pub struct JanusSession {
    url: String,
    tls: Option<native_tls::TlsConnector>,
    user: String,
    password: String,
    /// Traversal source bound to `g` in every request.
    source: Mutex<String>,
    ws: tokio::sync::Mutex<Option<Ws>>,
    cancel: CancelFlag,
}

/// Untyped JSON for a GraphSON 3 value; graph elements get the `~` markers.
fn plain(v: &Value) -> Value {
    match v {
        Value::Array(a) => Value::Array(a.iter().map(plain).collect()),
        Value::Object(o) => match (o.get("@type").and_then(Value::as_str), o.get("@value")) {
            (Some(t), Some(val)) => typed(t, val),
            _ => Value::Object(o.iter().map(|(k, v)| (k.clone(), plain(v))).collect()),
        },
        other => other.clone(),
    }
}

fn typed(t: &str, v: &Value) -> Value {
    match t {
        "g:List" | "g:Set" => plain(v),
        "g:BulkSet" => {
            let mut out = vec![];
            if let Some(a) = v.as_array() {
                for kv in a.chunks(2) {
                    let item = plain(&kv[0]);
                    let n = kv.get(1).map(plain).and_then(|b| b.as_u64()).unwrap_or(1);
                    out.extend(std::iter::repeat(item).take(n as usize));
                }
            }
            Value::Array(out)
        }
        "g:Map" => {
            let mut o = Map::new();
            if let Some(a) = v.as_array() {
                for kv in a.chunks(2) {
                    if let [k, val] = kv {
                        o.insert(key_str(k), plain(val));
                    }
                }
            }
            Value::Object(o)
        }
        "g:Date" | "g:Timestamp" => date_text(v).map(Value::String).unwrap_or_else(|| plain(v)),
        "g:Vertex" => {
            let mut o = Map::new();
            o.insert("~id".into(), id_val(v.get("id")));
            o.insert("~labels".into(), json!([v.get("label").map(plain).unwrap_or(Value::Null)]));
            props(&mut o, v.get("properties"));
            Value::Object(o)
        }
        "g:Edge" => {
            let mut o = Map::new();
            o.insert("~id".into(), id_val(v.get("id")));
            o.insert("~type".into(), v.get("label").map(plain).unwrap_or(Value::Null));
            o.insert("~start".into(), id_val(v.get("outV")));
            o.insert("~end".into(), id_val(v.get("inV")));
            props(&mut o, v.get("properties"));
            Value::Object(o)
        }
        "g:VertexProperty" | "g:Property" | "g:Traverser" => v.get("value").map(plain).unwrap_or(Value::Null),
        "g:Path" => {
            let objs = match v.get("objects").map(plain) {
                Some(Value::Array(a)) => a,
                _ => vec![],
            };
            if !objs.iter().any(|o| o.get("~id").is_some()) {
                return Value::Array(objs);
            }
            let (rels, nodes): (Vec<Value>, Vec<Value>) = objs.into_iter().partition(|o| o.get("~type").is_some());
            json!({ "~path": nodes, "~rels": rels })
        }
        "janusgraph:RelationIdentifier" => v.get("relationId").cloned().unwrap_or_else(|| plain(v)),
        _ => plain(v),
    }
}

/// Element ids as strings: JanusGraph ids are 64-bit and would lose precision in JS.
fn id_val(v: Option<&Value>) -> Value {
    match v.map(plain) {
        Some(Value::Number(n)) => Value::String(n.to_string()),
        Some(other) => other,
        None => Value::Null,
    }
}

fn props(o: &mut Map<String, Value>, p: Option<&Value>) {
    let Some(Value::Object(p)) = p else { return };
    for (k, vals) in p {
        // Vertex properties arrive as a list (multi-cardinality); unwrap the usual single value.
        let v = match plain(vals) {
            Value::Array(mut a) if a.len() == 1 => a.pop().unwrap(),
            other => other,
        };
        o.insert(k.clone(), v);
    }
}

fn key_str(k: &Value) -> String {
    match plain(k) {
        Value::String(s) => s,
        other => other.to_string(),
    }
}

fn date_text(v: &Value) -> Option<String> {
    let ms = v.as_i64()?;
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn cell(v: &Value) -> Cell {
    if let (Some(t), Some(val)) = (v.get("@type").and_then(Value::as_str), v.get("@value")) {
        match t {
            "g:Date" | "g:Timestamp" => {
                if let Some(s) = date_text(val) {
                    return Cell::DateTime(s);
                }
            }
            "gx:BigDecimal" | "gx:BigInteger" => return Cell::Dec(val.to_string().trim_matches('"').to_string()),
            _ => {}
        }
    }
    match plain(v) {
        Value::Null => Cell::Null,
        Value::Bool(b) => Cell::Bool(b),
        Value::Number(n) => n.as_i64().map(Cell::Int).or_else(|| n.as_f64().map(Cell::Float)).unwrap_or(Cell::Dec(n.to_string())),
        Value::String(s) => Cell::Text(s),
        other => Cell::Text(other.to_string()),
    }
}

fn map_pairs(v: &Value) -> Option<&Vec<Value>> {
    if v.get("@type")?.as_str()? == "g:Map" {
        v.get("@value")?.as_array()
    } else {
        None
    }
}

/// Items of a response's `result.data` (a g:List of results).
fn items(data: Option<&Value>) -> Vec<Value> {
    match data {
        None | Some(Value::Null) => vec![],
        Some(d) => match (d.get("@type").and_then(Value::as_str), d.get("@value"), d) {
            (Some("g:List"), Some(Value::Array(a)), _) => a.clone(),
            (_, _, Value::Array(a)) => a.clone(),
            _ => vec![d.clone()],
        },
    }
}

/// Turns result items into rows: maps (valueMap, project, select, elementMap…) become one
/// column per key, anything else a single `result` column.
#[derive(Default)]
struct Shaper {
    cols: Option<Vec<String>>,
    map_mode: bool,
}

impl Shaper {
    fn batch(&mut self, items: Vec<Value>, sink: &mut dyn RowSink) -> Result<bool> {
        if items.is_empty() {
            return Ok(true);
        }
        if self.cols.is_none() {
            let maps: Option<Vec<&Vec<Value>>> = items.iter().map(map_pairs).collect();
            let cols = match &maps {
                Some(maps) => {
                    self.map_mode = true;
                    let mut cols: Vec<String> = vec![];
                    for k in maps.iter().flat_map(|m| m.iter().step_by(2)).map(key_str) {
                        if !cols.contains(&k) {
                            cols.push(k);
                        }
                    }
                    cols
                }
                None => vec!["result".to_string()],
            };
            let first = self.cells(&cols, &items[0]);
            let metas = cols
                .iter()
                .zip(&first)
                .map(|(c, v)| ColumnMeta::new(c, "", matches!(v, Cell::Int(_) | Cell::Float(_) | Cell::Dec(_))))
                .collect();
            sink.columns(metas)?;
            self.cols = Some(cols);
        }
        let cols = self.cols.clone().unwrap();
        for it in &items {
            if !sink.row(self.cells(&cols, it))? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn cells(&self, cols: &[String], item: &Value) -> Vec<Cell> {
        if !self.map_mode {
            return vec![cell(item)];
        }
        match map_pairs(item) {
            Some(pairs) => {
                let m: HashMap<String, &Value> = pairs.chunks(2).filter(|kv| kv.len() == 2).map(|kv| (key_str(&kv[0]), &kv[1])).collect();
                cols.iter().map(|c| m.get(c).map(|v| cell(v)).unwrap_or(Cell::Null)).collect()
            }
            None => {
                let mut row = vec![Cell::Null; cols.len()];
                row[0] = cell(item);
                row
            }
        }
    }
}

/// Groovy single-quoted string literal.
fn groovy_str(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn frame(msg: &Value) -> Message {
    let body = msg.to_string();
    let mut buf = Vec::with_capacity(1 + MIME.len() + body.len());
    buf.push(MIME.len() as u8);
    buf.extend_from_slice(MIME.as_bytes());
    buf.extend_from_slice(body.as_bytes());
    Message::Binary(buf.into())
}

fn request_id(v: &Value) -> Option<&str> {
    let id = v.get("requestId")?;
    id.as_str().or_else(|| id.get("@value")?.as_str())
}

impl JanusSession {
    pub async fn connect(p: &ConnectionProfile, password: &str) -> Result<Self> {
        let host = if p.host.is_empty() { "localhost" } else { &p.host };
        let path = p.params.get("path").map(|s| s.trim()).filter(|s| !s.is_empty()).unwrap_or("/gremlin");
        let path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
        let (scheme, tls) = match p.ssl_mode.as_str() {
            "require" => (
                "wss",
                Some(native_tls::TlsConnector::builder().danger_accept_invalid_certs(true).danger_accept_invalid_hostnames(true).build()?),
            ),
            "verify-full" => ("wss", Some(native_tls::TlsConnector::new()?)),
            _ => ("ws", None),
        };
        let source = if p.database.trim().is_empty() { "g".to_string() } else { p.database.trim().to_string() };
        let s = Self {
            url: format!("{scheme}://{host}:{}{path}", p.port_or_default()),
            tls,
            user: p.username.clone(),
            password: password.to_string(),
            source: Mutex::new(source),
            ws: Default::default(),
            cancel: CancelFlag::default(),
        };
        s.server_version().await.context("Gremlin Server handshake failed")?;
        Ok(s)
    }

    async fn open(&self) -> Result<Ws> {
        let connector = self.tls.clone().map(tokio_tungstenite::Connector::NativeTls);
        let (ws, _) = tokio_tungstenite::connect_async_tls_with_config(self.url.as_str(), None, true, connector)
            .await
            .with_context(|| format!("Gremlin Server connect to {} failed", self.url))?;
        Ok(ws)
    }

    /// Evaluates a script, handing each response batch to `on_batch` (return false to stop).
    async fn submit(&self, script: &str, bindings: Value, mut on_batch: impl FnMut(Vec<Value>) -> Result<bool> + Send) -> Result<()> {
        let id = uuid::Uuid::new_v4().to_string();
        let source = self.source.lock().unwrap().clone();
        let req = json!({
            "requestId": id,
            "op": "eval",
            "processor": "",
            "args": { "gremlin": script, "language": "gremlin-groovy", "bindings": bindings, "aliases": { "g": source }, "batchSize": 500 },
        });
        let mut guard = self.ws.lock().await;
        let reused = guard.is_some();
        if guard.is_none() {
            *guard = Some(self.open().await?);
        }
        if let Err(e) = guard.as_mut().unwrap().send(frame(&req)).await {
            // An idle socket the server already closed: reconnect once (nothing was sent).
            if !reused {
                *guard = None;
                return Err(e.into());
            }
            let mut ws = self.open().await?;
            ws.send(frame(&req)).await?;
            *guard = Some(ws);
        }
        loop {
            if self.cancel.is_set() {
                // Remaining frames of this request would confuse the next one.
                *guard = None;
                return Ok(());
            }
            let ws = guard.as_mut().unwrap();
            let msg = match tokio::time::timeout(Duration::from_millis(250), ws.next()).await {
                Err(_) => continue,
                Ok(None) => {
                    *guard = None;
                    bail!("Gremlin Server connection closed");
                }
                Ok(Some(Err(e))) => {
                    *guard = None;
                    bail!("Gremlin Server connection closed: {e}");
                }
                Ok(Some(Ok(m))) => m,
            };
            let resp: Value = match &msg {
                Message::Text(t) => serde_json::from_str(t.as_str())?,
                Message::Binary(b) => serde_json::from_slice(b)?,
                Message::Close(_) => {
                    *guard = None;
                    bail!("Gremlin Server connection closed");
                }
                _ => continue,
            };
            if request_id(&resp).is_some_and(|r| r != id) {
                continue;
            }
            let status = &resp["status"];
            match status["code"].as_u64().unwrap_or(0) {
                code @ (200 | 206) => {
                    let more = on_batch(items(resp["result"].get("data")))?;
                    if code == 200 {
                        return Ok(());
                    }
                    if !more {
                        *guard = None;
                        return Ok(());
                    }
                }
                204 => return Ok(()),
                407 => {
                    let sasl = base64::engine::general_purpose::STANDARD.encode(format!("\0{}\0{}", self.user, self.password));
                    let auth = json!({
                        "requestId": id,
                        "op": "authentication",
                        "processor": "",
                        "args": { "sasl": sasl, "saslMechanism": "PLAIN" },
                    });
                    guard.as_mut().unwrap().send(frame(&auth)).await?;
                }
                code => {
                    let text = status["message"].as_str().filter(|m| !m.is_empty()).unwrap_or("request failed");
                    return Err(anyhow!("{text} (Gremlin status {code})"));
                }
            }
        }
    }

    async fn eval(&self, script: &str, bindings: Value) -> Result<Vec<Value>> {
        let mut out = vec![];
        self.submit(script, bindings, |b| {
            out.extend(b);
            Ok(true)
        })
        .await?;
        Ok(out)
    }

    fn strings(&self, v: Option<&Value>) -> Vec<String> {
        match v.map(plain) {
            Some(Value::Array(a)) => a.into_iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            _ => vec![],
        }
    }
}

#[async_trait]
impl Session for JanusSession {
    fn dialect(&self) -> Dialect {
        Dialect::Janus
    }
    fn cancel_flag(&self) -> &CancelFlag {
        &self.cancel
    }

    async fn run(&self, script: &str, sink: &mut dyn RowSink) -> Result<RunOutcome> {
        self.cancel.reset();
        let script = script.trim().trim_end_matches(';').trim();
        if let Some(src) = script.strip_prefix(":use ").or_else(|| script.strip_prefix(":USE ")) {
            *self.source.lock().unwrap() = src.trim().to_string();
            return Ok(RunOutcome { affected: Some(0) });
        }
        let mut shaper = Shaper::default();
        self.submit(script, json!({}), |items| shaper.batch(items, sink)).await?;
        if shaper.cols.is_none() {
            sink.columns(vec![ColumnMeta::new("result", "", false)])?;
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

    async fn ping(&self) -> Result<()> {
        let Ok(mut guard) = self.ws.try_lock() else { return Ok(()) };
        if let Some(ws) = guard.as_mut() {
            if ws.send(Message::Ping(Vec::new().into())).await.is_err() {
                *guard = None;
            }
        }
        Ok(())
    }

    async fn server_version(&self) -> Result<String> {
        let script = "def tp = org.apache.tinkerpop.gremlin.util.Gremlin.version()\n\
            try { 'JanusGraph ' + Class.forName('org.janusgraph.graphdb.configuration.JanusGraphConstants').VERSION + ' (TinkerPop ' + tp + ')' }\n\
            catch (Throwable e) { 'Gremlin Server (TinkerPop ' + tp + ')' }";
        let r = self.eval(script, json!({})).await?;
        Ok(r.first().and_then(|v| plain(v).as_str().map(String::from)).unwrap_or_else(|| "Gremlin Server".into()))
    }

    async fn native_current_schema(&self) -> Result<Option<String>> {
        Ok(Some(self.source.lock().unwrap().clone()))
    }

    async fn native_schemas(&self) -> Result<Option<Vec<String>>> {
        Ok(Some(vec![self.source.lock().unwrap().clone()]))
    }

    /// Browsing a traversal source makes it the session source (like `:use`).
    async fn native_objects(&self, schema: &str) -> Result<Option<Vec<DbObject>>> {
        *self.source.lock().unwrap() = schema.to_string();
        // JanusGraph schema management; plain Gremlin Servers fall back to sampling.
        let script = "try {\n\
              def m = g.getGraph().openManagement()\n\
              try { [m.getVertexLabels().collect { it.name() }.sort(), m.getRelationTypes(Class.forName('org.janusgraph.core.EdgeLabel')).collect { it.name() }.sort()] }\n\
              finally { m.rollback() }\n\
            } catch (Throwable e) {\n\
              [g.V().limit(10000).label().dedup().toList().sort(), g.E().limit(10000).label().dedup().toList().sort()]\n\
            }";
        let r = self.eval(script, json!({})).await?;
        let mut out = vec![];
        for name in self.strings(r.first()) {
            out.push(DbObject { schema: schema.into(), name, kind: "label".into() });
        }
        for name in self.strings(r.get(1)) {
            out.push(DbObject { schema: schema.into(), name, kind: "relationship".into() });
        }
        Ok(Some(out))
    }

    async fn native_columns(&self, _schema: &str, name: &str) -> Result<Option<Vec<ColumnInfo>>> {
        let script = "def keys = g.V().hasLabel(L).limit(200).properties().key().dedup().toList()\n\
            if (keys.isEmpty()) keys = g.E().hasLabel(L).limit(200).properties().key().dedup().toList()\n\
            def types = [:]\n\
            try { def m = g.getGraph().openManagement(); try { keys.each { k -> def pk = m.getPropertyKey(k); if (pk) types[k] = pk.dataType().simpleName } } finally { m.rollback() } } catch (Throwable e) {}\n\
            keys.sort().collect { [it, types[it] ?: 'property'] }";
        let r = self.eval(script, json!({ "L": name })).await?;
        Ok(Some(
            r.iter()
                .filter_map(|kv| match plain(kv) {
                    Value::Array(a) if a.len() == 2 => Some((a[0].as_str()?.to_string(), a[1].as_str().unwrap_or("property").to_string())),
                    _ => None,
                })
                .map(|(name, data_type)| ColumnInfo { name, data_type, nullable: true, default_value: None, primary_key: false, comment: None })
                .collect(),
        ))
    }

    async fn native_ddl(&self, _schema: &str, name: &str, _kind: &str) -> Result<Option<String>> {
        let script = format!(
            "try {{ def m = g.getGraph().openManagement(); try {{ m.printSchema() }} finally {{ m.rollback() }} }}\n\
             catch (Throwable e) {{ '// Schema for ' + {} + ' unavailable (not a JanusGraph server?): ' + e.message }}",
            groovy_str(name)
        );
        let r = self.eval(&script, json!({})).await?;
        Ok(Some(r.first().map(|v| key_str(v)).unwrap_or_default()))
    }

    async fn graph_edges(&self, ids: &[String], limit: usize) -> Result<Vec<Value>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        // Ids travel as strings (64-bit safe); numeric ones go back to Long for lookup.
        let script = "def ids = I.collect { it.isLong() ? it.toLong() : it }
            g.V(ids).outE().where(inV().hasId(within(ids))).dedup().limit(N).toList()";
        let bindings = json!({
            "I": { "@type": "g:List", "@value": ids },
            "N": { "@type": "g:Int64", "@value": limit },
        });
        let r = self.eval(script, bindings).await?;
        Ok(r.iter().map(plain).filter(|e| e.get("~start").is_some()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rows(Vec<String>, Vec<Vec<Cell>>);
    impl RowSink for Rows {
        fn columns(&mut self, cols: Vec<ColumnMeta>) -> Result<()> {
            self.0 = cols.into_iter().map(|c| c.name).collect();
            Ok(())
        }
        fn row(&mut self, row: Vec<Cell>) -> Result<bool> {
            self.1.push(row);
            Ok(true)
        }
    }

    #[test]
    fn graphson_elements() {
        let v: Value = serde_json::from_str(
            r#"{"@type":"g:Vertex","@value":{"id":{"@type":"g:Int64","@value":4128},"label":"person",
              "properties":{"name":[{"@type":"g:VertexProperty","@value":{"id":{"@type":"janusgraph:RelationIdentifier","@value":{"relationId":"16p-36o-sl"}},"value":"Ann","label":"name"}}]}}}"#,
        )
        .unwrap();
        assert_eq!(plain(&v), json!({"~id":"4128","~labels":["person"],"name":"Ann"}));
        let e: Value = serde_json::from_str(
            r#"{"@type":"g:Edge","@value":{"id":{"@type":"janusgraph:RelationIdentifier","@value":{"relationId":"2dc-36o-2dx"}},
              "label":"knows","inVLabel":"person","outVLabel":"person","inV":{"@type":"g:Int64","@value":8224},"outV":{"@type":"g:Int64","@value":4128},
              "properties":{"since":{"@type":"g:Property","@value":{"key":"since","value":{"@type":"g:Int32","@value":2020}}}}}}"#,
        )
        .unwrap();
        assert_eq!(plain(&e), json!({"~id":"2dc-36o-2dx","~type":"knows","~start":"4128","~end":"8224","since":2020}));
    }

    #[test]
    fn shapes_maps_into_columns() {
        let items: Vec<Value> = serde_json::from_str(
            r#"[{"@type":"g:Map","@value":["name","Ann","age",{"@type":"g:Int32","@value":30}]},
                {"@type":"g:Map","@value":["name","Bob",{"@type":"g:T","@value":"id"},{"@type":"g:Int64","@value":7}]}]"#,
        )
        .unwrap();
        let mut sink = Rows(vec![], vec![]);
        let mut s = Shaper::default();
        s.batch(items, &mut sink).unwrap();
        assert_eq!(sink.0, vec!["name", "age", "id"]);
        assert_eq!(sink.1[0], vec![Cell::Text("Ann".into()), Cell::Int(30), Cell::Null]);
        assert_eq!(sink.1[1], vec![Cell::Text("Bob".into()), Cell::Null, Cell::Int(7)]);
        let mut sink = Rows(vec![], vec![]);
        Shaper::default().batch(vec![json!({"@type":"g:Int64","@value":6}), json!("x")], &mut sink).unwrap();
        assert_eq!(sink.0, vec!["result"]);
        assert_eq!(sink.1, vec![vec![Cell::Int(6)], vec![Cell::Text("x".into())]]);
    }

    #[test]
    fn dates_and_bulksets() {
        assert_eq!(cell(&json!({"@type":"g:Date","@value":0})), Cell::DateTime("1970-01-01T00:00:00.000Z".into()));
        assert_eq!(plain(&json!({"@type":"g:BulkSet","@value":["a",{"@type":"g:Int64","@value":2}]})), json!(["a", "a"]));
        assert_eq!(groovy_str("it's \\"), r"'it\'s \\'");
    }
}
