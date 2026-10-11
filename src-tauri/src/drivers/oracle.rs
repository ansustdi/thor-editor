use super::{leading_keyword, CancelFlag, Dialect, RowSink, RunOutcome, Session};
use crate::model::{Cell, ColumnMeta, ConnectionProfile};
use crate::oracle_env;
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use oracle::sql_type::OracleType;
use oracle::{Connection, Connector, InitParams, Privilege, SqlValue};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// OCI can be initialised once per process; remember which client was used.
static INIT: OnceLock<std::result::Result<String, String>> = OnceLock::new();

fn ensure_init(lib_dir: &str, config_dir: &str) -> Result<()> {
    let wanted = lib_dir.to_string();
    let res = INIT.get_or_init(|| {
        let mut p = InitParams::new();
        let r: oracle::Result<()> = (|| {
            if !lib_dir.is_empty() {
                p.oracle_client_lib_dir(lib_dir)?;
            }
            if !config_dir.is_empty() {
                p.oracle_client_config_dir(config_dir)?;
            }
            p.init()?;
            Ok(())
        })();
        r.map(|_| wanted.clone()).map_err(|e| e.to_string())
    });
    match res {
        Ok(used) if wanted.is_empty() || used.eq_ignore_ascii_case(&wanted) => Ok(()),
        Ok(used) => bail!(
            "Oracle client already loaded from '{}'. Restart the app to switch to '{}'.",
            if used.is_empty() { "system PATH" } else { used },
            if wanted.is_empty() { "system PATH" } else { &wanted }
        ),
        Err(e) => bail!("Failed to load Oracle client: {e}"),
    }
}

/// "client 23.9.0.25.7 (bundled Instant Client)" — which OCI library this process loaded.
pub fn client_description() -> String {
    let version = oracle::Version::client().map(|v| v.to_string()).unwrap_or_else(|_| "?".into());
    let used = INIT.get().and_then(|r| r.as_ref().ok()).cloned().unwrap_or_default();
    let bundled = crate::oracle_env::bundled_dir()
        .map(|b| !used.is_empty() && std::path::Path::new(&used) == b.as_path())
        .unwrap_or(false);
    let source = if bundled {
        "bundled Instant Client".to_string()
    } else if used.is_empty() {
        "client from PATH".to_string()
    } else {
        used
    };
    format!("client {version} ({source})")
}

pub struct OraSession {
    conn: Connection,
    run_lock: Mutex<()>,
    cancel: CancelFlag,
    autocommit: AtomicBool,
    server_output: AtomicBool,
}

/// Drains the whole DBMS_OUTPUT buffer into one CLOB: a single round trip instead of one per line.
const DRAIN_OUTPUT: &str = "DECLARE l_line VARCHAR2(32767); l_status INTEGER; l_buf CLOB; \
BEGIN LOOP DBMS_OUTPUT.GET_LINE(l_line, l_status); EXIT WHEN l_status = 1; \
l_buf := l_buf || l_line || CHR(10); END LOOP; :out := l_buf; END;";

fn connect_string(p: &ConnectionProfile) -> Result<String> {
    let o = &p.oracle;
    Ok(match o.method.as_str() {
        "tns" => {
            if o.tns_alias.is_empty() {
                bail!("TNS alias is required");
            }
            o.tns_alias.clone()
        }
        "ldap" => {
            if o.ldap_alias.is_empty() {
                bail!("LDAP service name is required");
            }
            o.ldap_alias.clone()
        }
        _ => {
            let host = if p.host.is_empty() { "localhost" } else { &p.host };
            let port = p.port_or_default();
            let svc = if o.service.is_empty() { &p.database } else { &o.service };
            if svc.is_empty() {
                bail!("Service name / SID is required");
            }
            if o.service_type == "sid" {
                format!("(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST={host})(PORT={port}))(CONNECT_DATA=(SID={svc})))")
            } else {
                format!("//{host}:{port}/{svc}")
            }
        }
    })
}

impl OraSession {
    pub async fn connect(p: &ConnectionProfile, password: &str) -> Result<Self> {
        let o = &p.oracle;
        let lib_dir = if !o.client_lib_dir.is_empty() {
            o.client_lib_dir.clone()
        } else {
            oracle_env::bundled_dir().map(|d| d.to_string_lossy().to_string()).unwrap_or_default()
        };
        let config_dir = if !o.config_dir.is_empty() {
            o.config_dir.clone()
        } else if !lib_dir.is_empty() {
            oracle_env::config_dir_for(&PathBuf::from(&lib_dir))
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let cs = connect_string(p)?;
        let user = p.username.clone();
        let pass = password.to_string();
        let privilege = match o.connect_as.to_ascii_lowercase().as_str() {
            "sysdba" => Some(Privilege::Sysdba),
            "sysoper" => Some(Privilege::Sysoper),
            "sysasm" => Some(Privilege::Sysasm),
            "sysbackup" => Some(Privilege::Sysbackup),
            "sysdg" => Some(Privilege::Sysdg),
            "syskm" => Some(Privilege::Syskm),
            _ => None,
        };
        let external = user.is_empty() || user == "/";
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            ensure_init(&lib_dir, &config_dir)?;
            let mut c = Connector::new(if external { "" } else { &user }, if external { "" } else { &pass }, &cs);
            if external {
                c.external_auth(true);
            }
            if let Some(pr) = privilege {
                c.privilege(pr);
            }
            c.connect().context("Oracle connect failed")
        })
        .await??;
        Ok(Self {
            conn,
            run_lock: Mutex::new(()),
            cancel: CancelFlag::default(),
            autocommit: AtomicBool::new(false),
            server_output: AtomicBool::new(false),
        })
    }

    fn run_blocking(&self, sql: &str, sink: &mut dyn RowSink) -> Result<RunOutcome> {
        let _g = self.run_lock.lock().map_err(|_| anyhow!("session poisoned"))?;
        let sql = normalize(sql);
        let mut stmt = self.conn.statement(&sql).fetch_array_size(500).build()?;
        if !stmt.is_query() {
            stmt.execute(&[])?;
            let n = stmt.row_count().unwrap_or(0);
            if self.autocommit.load(Ordering::SeqCst) {
                self.conn.commit()?;
            }
            return Ok(RunOutcome { affected: Some(n) });
        }
        let rows = stmt.query(&[])?;
        let types: Vec<OracleType> = rows.column_info().iter().map(|c| c.oracle_type().clone()).collect();
        sink.columns(
            rows.column_info()
                .iter()
                .map(|c| {
                    let t = c.oracle_type();
                    ColumnMeta::new(c.name(), t.to_string(), is_numeric(t))
                })
                .collect(),
        )?;
        for row in rows {
            let row = row?;
            let cells = row.sql_values().iter().zip(&types).map(|(v, t)| to_cell(v, t)).collect();
            if !sink.row(cells)? || self.cancel.is_set() {
                break;
            }
        }
        Ok(RunOutcome { affected: None })
    }
}

fn is_plsql(sql: &str) -> bool {
    let kw = leading_keyword(sql);
    if matches!(kw.as_str(), "begin" | "declare") {
        return true;
    }
    if kw == "create" {
        let low = sql.to_ascii_lowercase();
        let head: String = low.split_whitespace().take(6).collect::<Vec<_>>().join(" ");
        return ["procedure", "function", "package", "trigger", "type "]
            .iter()
            .any(|k| head.contains(k));
    }
    false
}

/// Strip the trailing `;` Oracle rejects on plain SQL (PL/SQL keeps it) and a lone `/`.
fn normalize(sql: &str) -> String {
    let mut s = sql.trim().trim_end_matches('/').trim_end().to_string();
    if !is_plsql(&s) {
        while s.ends_with(';') {
            s.pop();
        }
    }
    s
}

fn is_numeric(t: &OracleType) -> bool {
    matches!(
        t,
        OracleType::Number(..) | OracleType::Float(_) | OracleType::BinaryFloat | OracleType::BinaryDouble | OracleType::Int64 | OracleType::UInt64
    )
}

fn fix_number(s: String) -> String {
    if let Some(r) = s.strip_prefix("-.") {
        format!("-0.{r}")
    } else if s.starts_with('.') {
        format!("0{s}")
    } else {
        s
    }
}

fn to_cell(v: &SqlValue, t: &OracleType) -> Cell {
    if v.is_null().unwrap_or(false) {
        return Cell::Null;
    }
    let text = || v.get::<String>().map(Cell::Text).unwrap_or(Cell::Text(format!("<{t}>")));
    match t {
        OracleType::Number(..) | OracleType::Float(_) | OracleType::Int64 | OracleType::UInt64 => {
            match v.get::<String>() {
                Ok(s) => {
                    let s = fix_number(s);
                    if !s.contains(['.', 'E', 'e']) {
                        s.parse().map(Cell::Int).unwrap_or(Cell::Dec(s))
                    } else {
                        Cell::Dec(s)
                    }
                }
                Err(_) => text(),
            }
        }
        OracleType::BinaryFloat | OracleType::BinaryDouble => v.get::<f64>().map(Cell::Float).unwrap_or_else(|_| text()),
        OracleType::Date | OracleType::Timestamp(_) => v
            .get::<chrono::NaiveDateTime>()
            .map(|d| Cell::DateTime(d.format("%Y-%m-%d %H:%M:%S%.f").to_string()))
            .unwrap_or_else(|_| text()),
        OracleType::TimestampTZ(_) | OracleType::TimestampLTZ(_) => v
            .get::<chrono::DateTime<chrono::FixedOffset>>()
            .map(|d| Cell::DateTime(d.to_rfc3339()))
            .unwrap_or_else(|_| text()),
        OracleType::Raw(_) | OracleType::BLOB | OracleType::LongRaw | OracleType::BFILE => {
            v.get::<Vec<u8>>().map(Cell::Bytes).unwrap_or_else(|_| text())
        }
        OracleType::Boolean => v.get::<bool>().map(Cell::Bool).unwrap_or_else(|_| text()),
        _ => text(),
    }
}

#[async_trait]
impl Session for OraSession {
    fn dialect(&self) -> Dialect {
        Dialect::Oracle
    }
    fn cancel_flag(&self) -> &CancelFlag {
        &self.cancel
    }

    async fn run(&self, sql: &str, sink: &mut dyn RowSink) -> Result<RunOutcome> {
        self.cancel.reset();
        tokio::task::block_in_place(|| self.run_blocking(sql, sink))
    }

    async fn cancel(&self) -> Result<()> {
        self.cancel.set();
        self.conn.break_execution()?;
        Ok(())
    }

    async fn set_autocommit(&self, on: bool) -> Result<()> {
        self.autocommit.store(on, Ordering::SeqCst);
        Ok(())
    }

    async fn commit(&self) -> Result<()> {
        tokio::task::block_in_place(|| self.conn.commit())?;
        Ok(())
    }

    async fn rollback(&self) -> Result<()> {
        tokio::task::block_in_place(|| self.conn.rollback())?;
        Ok(())
    }

    async fn set_server_output(&self, on: bool) -> Result<bool> {
        let sql = if on { "BEGIN DBMS_OUTPUT.ENABLE(NULL); END;" } else { "BEGIN DBMS_OUTPUT.DISABLE; END;" };
        self.exec(sql).await?;
        self.server_output.store(on, Ordering::SeqCst);
        Ok(true)
    }

    async fn take_server_output(&self) -> Result<Vec<String>> {
        if !self.server_output.load(Ordering::SeqCst) {
            return Ok(vec![]);
        }
        let text: Option<String> = tokio::task::block_in_place(|| -> Result<Option<String>> {
            let _g = self.run_lock.lock().map_err(|_| anyhow!("session poisoned"))?;
            let mut st = self.conn.statement(DRAIN_OUTPUT).build()?;
            st.bind(1, &OracleType::CLOB)?;
            st.execute(&[])?;
            Ok(st.bind_value(1)?)
        })?;
        let mut lines: Vec<String> = text.unwrap_or_default().split('\n').map(str::to_string).collect();
        if lines.last().map(|l| l.is_empty()).unwrap_or(false) {
            lines.pop();
        }
        Ok(lines)
    }

    async fn ping(&self) -> Result<()> {
        // Skip while a statement runs; `select 1 from dual` also resets the server IDLE_TIME clock.
        let Ok(_g) = self.run_lock.try_lock() else { return Ok(()) };
        tokio::task::block_in_place(|| self.conn.query_row("select 1 from dual", &[]).map(|_| ()))?;
        Ok(())
    }

    async fn server_version(&self) -> Result<String> {
        let (v, banner) = tokio::task::block_in_place(|| self.conn.server_version())?;
        Ok(format!("Oracle {v} {} · {}", banner.split_whitespace().collect::<Vec<_>>().join(" "), client_description()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_sql_semicolon_only() {
        assert_eq!(normalize("select * from dual;"), "select * from dual");
        assert_eq!(normalize("begin null; end;\n/"), "begin null; end;");
        assert_eq!(normalize("create or replace procedure p as begin null; end;"), "create or replace procedure p as begin null; end;");
        assert_eq!(fix_number(".5".into()), "0.5");
        assert_eq!(fix_number("-.25".into()), "-0.25");
    }
}

#[cfg(test)]
mod client_load {
    use crate::model::ConnectionProfile;

    #[tokio::test(flavor = "multi_thread")]
    async fn oci_loads_and_reports_network_error() {
        let Some(home) = crate::oracle_env::list_homes().into_iter().next() else { return };
        let p: ConnectionProfile = serde_json::from_value(serde_json::json!({
            "id": "t", "engine": "oracle", "host": "127.0.0.1", "port": 1,
            "username": "x", "oracle": { "method": "direct", "service": "NOPE", "clientLibDir": home.lib_dir }
        }))
        .unwrap();
        let err = super::OraSession::connect(&p, "x").await.err().expect("must fail");
        let full = format!("{err:#}");
        eprintln!("{full}");
        assert!(!full.contains("DPI-1047"), "client failed to load: {full}");
    }
}
