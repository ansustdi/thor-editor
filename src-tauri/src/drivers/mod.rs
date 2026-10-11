pub mod clickhouse;
pub mod elastic;
pub mod janusgraph;
pub mod mongo;
pub mod mssql;
pub mod mysql;
pub mod neo4j;
pub mod redis;
pub mod oracle;
pub mod postgres;
pub mod sqlite;

use crate::model::{Cell, ColumnInfo, ColumnMeta, ConnectionProfile, DbObject, Engine};
use crate::ssh::Tunnel;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Receives a streamed result set. Returning `false` from `row` stops fetching.
pub trait RowSink: Send {
    fn columns(&mut self, cols: Vec<ColumnMeta>) -> Result<()>;
    fn row(&mut self, row: Vec<Cell>) -> Result<bool>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RunOutcome {
    pub affected: Option<u64>,
}

#[derive(Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst)
    }
    pub fn set(&self) {
        self.0.store(true, Ordering::SeqCst)
    }
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    Mysql,
    Sqlite,
    Mssql,
    Oracle,
    Clickhouse,
    Redis,
    Mongo,
    Neo4j,
    Janus,
    Elastic,
}

#[async_trait]
pub trait Session: Send + Sync {
    fn dialect(&self) -> Dialect;
    /// Flag checked by drivers between rows; set by `cancel`.
    fn cancel_flag(&self) -> &CancelFlag;

    /// Execute a single statement, streaming rows to `sink`.
    async fn run(&self, sql: &str, sink: &mut dyn RowSink) -> Result<RunOutcome>;

    /// Best-effort server-side cancel of the running statement.
    async fn cancel(&self) -> Result<()> {
        self.cancel_flag().set();
        Ok(())
    }

    async fn set_autocommit(&self, on: bool) -> Result<()>;
    async fn commit(&self) -> Result<()> {
        self.exec("COMMIT").await
    }
    async fn rollback(&self) -> Result<()> {
        self.exec("ROLLBACK").await
    }

    async fn exec(&self, sql: &str) -> Result<()> {
        let mut sink = CollectSink::new(0);
        self.run(sql, &mut sink).await?;
        Ok(())
    }

    async fn server_version(&self) -> Result<String>;

    /// Cheap round trip used by the keep-alive task. Must not wait behind a running query
    /// (skip instead). Engines with pooled/auto-reconnecting clients keep the default.
    async fn ping(&self) -> Result<()> {
        Ok(())
    }

    /// Server output (Oracle DBMS_OUTPUT). Returns false when the engine has none.
    async fn set_server_output(&self, _on: bool) -> Result<bool> {
        Ok(false)
    }

    /// Lines produced since the last call (empty when disabled / unsupported).
    async fn take_server_output(&self) -> Result<Vec<String>> {
        Ok(vec![])
    }

    // Non-SQL engines answer metadata natively; `None` falls back to catalog SQL in `meta`.
    async fn native_current_schema(&self) -> Result<Option<String>> {
        Ok(None)
    }
    async fn native_schemas(&self) -> Result<Option<Vec<String>>> {
        Ok(None)
    }
    async fn native_objects(&self, _schema: &str) -> Result<Option<Vec<DbObject>>> {
        Ok(None)
    }
    async fn native_columns(&self, _schema: &str, _name: &str) -> Result<Option<Vec<ColumnInfo>>> {
        Ok(None)
    }
    async fn native_ddl(&self, _schema: &str, _name: &str, _kind: &str) -> Result<Option<String>> {
        Ok(None)
    }

    /// Graph engines: edges whose both endpoints are among `ids` (node `~id`s), as JSON with
    /// `~id`/`~type`/`~start`/`~end` markers. Lets the graph view connect returned nodes.
    async fn graph_edges(&self, _ids: &[String], _limit: usize) -> Result<Vec<serde_json::Value>> {
        Ok(vec![])
    }
}

pub struct Connected {
    pub session: Arc<dyn Session>,
    /// Kept alive for the life of the session when connecting through SSH.
    pub tunnel: Option<Tunnel>,
}

pub async fn connect(p: &ConnectionProfile, password: &str, ssh_secret: &str) -> Result<Connected> {
    let mut tunnel = None;
    let mut profile = p.clone();
    if p.ssh.enabled && p.engine != Engine::Sqlite {
        if p.engine == Engine::Oracle && !matches!(p.oracle.method.as_str(), "" | "direct") {
            anyhow::bail!("SSH tunnels need the Oracle Direct connection method (TNS/LDAP resolve on the client)");
        }
        let host = if p.host.is_empty() { "localhost" } else { &p.host };
        let t = crate::ssh::open(&p.ssh, ssh_secret, host, p.port_or_default()).await?;
        profile.host = "127.0.0.1".into();
        profile.port = t.local_port;
        tunnel = Some(t);
    }
    let session = open_session(&profile, password).await?;
    Ok(Connected { session, tunnel })
}

async fn open_session(p: &ConnectionProfile, password: &str) -> Result<Arc<dyn Session>> {
    let s: Arc<dyn Session> = match p.engine {
        Engine::Postgres => Arc::new(postgres::PgSession::connect(p, password).await?),
        Engine::Mysql | Engine::Mariadb => Arc::new(mysql::MySession::connect(p, password).await?),
        Engine::Sqlite => Arc::new(sqlite::LiteSession::connect(p).await?),
        Engine::Mssql => Arc::new(mssql::MsSession::connect(p, password).await?),
        Engine::Oracle => Arc::new(oracle::OraSession::connect(p, password).await?),
        Engine::Clickhouse => Arc::new(clickhouse::ChSession::connect(p, password).await?),
        Engine::Redis => Arc::new(redis::RedisSession::connect(p, password).await?),
        Engine::Mongodb => Arc::new(mongo::MongoSession::connect(p, password).await?),
        Engine::Neo4j => Arc::new(neo4j::NeoSession::connect(p, password).await?),
        Engine::Janusgraph => Arc::new(janusgraph::JanusSession::connect(p, password).await?),
        Engine::Elasticsearch => Arc::new(elastic::EsSession::connect(p, password).await?),
    };
    s.set_autocommit(p.autocommit_default()).await?;
    Ok(s)
}

/// Collects up to `limit` rows (0 = unlimited) into memory.
pub struct CollectSink {
    pub limit: usize,
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Cell>>,
    pub truncated: bool,
}

impl CollectSink {
    pub fn new(limit: usize) -> Self {
        Self { limit, columns: vec![], rows: vec![], truncated: false }
    }
}

impl RowSink for CollectSink {
    fn columns(&mut self, cols: Vec<ColumnMeta>) -> Result<()> {
        self.columns = cols;
        Ok(())
    }
    fn row(&mut self, row: Vec<Cell>) -> Result<bool> {
        if self.limit > 0 && self.rows.len() >= self.limit {
            self.truncated = true;
            return Ok(false);
        }
        self.rows.push(row);
        Ok(true)
    }
}

/// First SQL keyword, lowercased, skipping comments and parentheses.
pub fn leading_keyword(sql: &str) -> String {
    let mut s = sql.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.split_once('\n').map(|x| x.1).unwrap_or("").trim_start();
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.split_once("*/").map(|x| x.1).unwrap_or("").trim_start();
        } else if let Some(rest) = s.strip_prefix('(') {
            s = rest.trim_start();
        } else {
            break;
        }
    }
    s.chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_lowercase()
}

/// Statements that are safe to re-run automatically after a reconnect.
pub fn is_read_only(sql: &str) -> bool {
    matches!(leading_keyword(sql).as_str(), "select" | "with" | "show" | "explain" | "describe" | "desc" | "values" | "table")
}

/// Errors meaning the server connection is gone (idle timeout, killed session, network drop).
pub fn is_connection_lost(msg: &str) -> bool {
    const PATTERNS: &[&str] = &[
        // Oracle / ODPI-C
        "DPI-1010", "DPI-1080", "ORA-03113", "ORA-03114", "ORA-03135", "ORA-02396", "ORA-01012", "ORA-00028",
        "ORA-12537", "ORA-12547", "ORA-03136", "ORA-12170", "ORA-25408",
        // PostgreSQL (tokio-postgres)
        "connection closed", "terminating connection", "server closed the connection",
        // MySQL / MariaDB
        "server has gone away", "Lost connection to MySQL", "Connection reset", "broken pipe", "Broken pipe",
        "connection was killed", "Connection was killed",
        // SQL Server / sockets
        "forcibly closed", "os error 10054", "os error 10053", "os error 32", "ConnectionReset", "ConnectionAborted",
        "unexpected end of file",
    ];
    PATTERNS.iter().any(|p| msg.contains(p))
}

pub fn is_dml(sql: &str) -> bool {
    matches!(leading_keyword(sql).as_str(), "insert" | "update" | "delete" | "merge")
}

/// Quote a string literal ('' escaping) for metadata queries.
pub fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_skips_comments() {
        assert_eq!(leading_keyword("-- hi\n /* x */ ( SELECT 1)"), "select");
        assert_eq!(leading_keyword("update t set a=1"), "update");
        assert!(is_dml("  DELETE FROM x"));
        assert!(!is_dml("with x as (select 1) select * from x"));
    }

    #[test]
    fn lost_connection_patterns() {
        assert!(is_connection_lost("DPI Error: DPI-1080: connection was closed by ORA-03135"));
        assert!(is_connection_lost("DPI Error: DPI-1010: not connected"));
        assert!(is_connection_lost("db error: connection closed"));
        assert!(!is_connection_lost("ORA-00942: table or view does not exist"));
        assert!(is_read_only("/* Formatted on x */\nSELECT * FROM gen_config"));
        assert!(!is_read_only("update t set a = 1"));
    }

    #[test]
    fn literal_escape() {
        assert_eq!(lit("O'Brien"), "'O''Brien'");
    }
}

#[cfg(test)]
mod sqlite_it {
    use super::*;
    use crate::export::{ExportOptions, Exporter, SharedBuf, Target};
    use crate::model::{ConnectionProfile, Engine};

    fn profile(path: &str) -> ConnectionProfile {
        serde_json::from_value(serde_json::json!({ "id": "t", "engine": "sqlite", "database": path })).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sqlite_end_to_end() {
        let dir = std::env::temp_dir().join(format!("dbforge-it-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("t.db");
        let p = profile(db.to_str().unwrap());
        assert_eq!(p.engine, Engine::Sqlite);
        let s = connect(&p, "", "").await.unwrap().session;

        s.exec("create table emp (id integer primary key, name text not null, salary real, photo blob)").await.unwrap();
        let mut sink = CollectSink::new(0);
        let out = s
            .run("insert into emp values (1,'Ann',100.5,x'CAFE'),(2,'Bo''b',null,null),(3,'Cy',3,null)", &mut sink)
            .await
            .unwrap();
        assert_eq!(out.affected, Some(3));

        let mut sink = CollectSink::new(2);
        s.run("select * from emp order by id", &mut sink).await.unwrap();
        assert_eq!(sink.columns.len(), 4);
        assert!(sink.columns[2].numeric);
        assert_eq!(sink.rows.len(), 2);
        assert!(sink.truncated);
        assert_eq!(sink.rows[0][3], Cell::Bytes(vec![0xCA, 0xFE]));
        assert_eq!(sink.rows[1][1], Cell::Text("Bo'b".into()));

        // manual commit + rollback
        s.set_autocommit(false).await.unwrap();
        s.exec("delete from emp").await.unwrap();
        s.rollback().await.unwrap();
        let mut sink = CollectSink::new(0);
        s.run("select count(*) from emp", &mut sink).await.unwrap();
        assert_eq!(sink.rows[0][0], Cell::Int(3));

        // metadata
        let objs = crate::meta::list_objects(s.as_ref(), "main").await.unwrap();
        assert!(objs.iter().any(|o| o.name == "emp" && o.kind == "table"));
        let cols = crate::meta::columns(s.as_ref(), "main", "emp").await.unwrap();
        assert!(cols[0].primary_key && !cols[1].nullable);
        let ddl = crate::meta::ddl(s.as_ref(), "main", "emp", "table").await.unwrap();
        assert!(ddl.to_lowercase().contains("create table emp"));

        // streamed export straight from a query
        let buf = SharedBuf::default();
        let o = ExportOptions { format: "sql".into(), table_name: "emp".into(), dialect: "sqlite".into(), ..Default::default() };
        let mut e = Exporter::new(o, Target::Buffer(buf.clone()), None).unwrap();
        s.run("select id, name, photo from emp order by id", &mut e).await.unwrap();
        assert_eq!(e.finish().unwrap(), 3);
        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(text.contains("INSERT INTO emp (id, name, photo) VALUES (1, 'Ann', X'CAFE');"));
        assert!(text.contains("(2, 'Bo''b', NULL)"));

        // xlsx to file
        let xlsx = dir.join("out.xlsx");
        let o = ExportOptions { format: "xlsx".into(), ..Default::default() };
        let mut e = Exporter::new(o, Target::File(xlsx.clone()), None).unwrap();
        s.run("select * from emp", &mut e).await.unwrap();
        e.finish().unwrap();
        assert!(std::fs::metadata(&xlsx).unwrap().len() > 1000);

        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_oracle_homes() {
        let homes = crate::oracle_env::list_homes();
        eprintln!("{homes:#?}");
    }
}
