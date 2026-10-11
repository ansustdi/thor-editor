use super::{CancelFlag, Dialect, RowSink, RunOutcome, Session};
use crate::model::{Cell, ColumnMeta, ConnectionProfile};
use anyhow::{Context, Result};
use async_trait::async_trait;
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::prelude::*;
use mysql_async::{Conn, Opts, OptsBuilder, SslOpts, Value};
use tokio::sync::Mutex;

pub struct MySession {
    conn: Mutex<Conn>,
    opts: Opts,
    conn_id: u32,
    cancel: CancelFlag,
}

/// charset id 63 = binary
const BINARY_CHARSET: u16 = 63;

impl MySession {
    pub async fn connect(p: &ConnectionProfile, password: &str) -> Result<Self> {
        let mut b = OptsBuilder::default()
            .ip_or_hostname(if p.host.is_empty() { "localhost".to_string() } else { p.host.clone() })
            .tcp_port(p.port_or_default())
            .user(Some(p.username.clone()))
            .pass(Some(password.to_string()))
            .prefer_socket(false);
        if !p.database.is_empty() {
            b = b.db_name(Some(p.database.clone()));
        }
        if matches!(p.ssl_mode.as_str(), "prefer" | "require" | "verify-full") {
            b = b.ssl_opts(Some(
                SslOpts::default().with_danger_accept_invalid_certs(p.ssl_mode != "verify-full"),
            ));
        }
        let opts: Opts = b.into();
        let conn = Conn::new(opts.clone()).await.context("MySQL connect failed")?;
        let conn_id = conn.id();
        Ok(Self { conn: Mutex::new(conn), opts, conn_id, cancel: CancelFlag::default() })
    }
}

fn numeric(t: ColumnType) -> bool {
    use ColumnType::*;
    matches!(
        t,
        MYSQL_TYPE_TINY
            | MYSQL_TYPE_SHORT
            | MYSQL_TYPE_LONG
            | MYSQL_TYPE_LONGLONG
            | MYSQL_TYPE_INT24
            | MYSQL_TYPE_FLOAT
            | MYSQL_TYPE_DOUBLE
            | MYSQL_TYPE_DECIMAL
            | MYSQL_TYPE_NEWDECIMAL
            | MYSQL_TYPE_YEAR
    )
}

fn type_name(t: ColumnType) -> String {
    format!("{t:?}").trim_start_matches("MYSQL_TYPE_").to_ascii_lowercase()
}

fn to_cell(v: Value, t: ColumnType, binary: bool) -> Cell {
    use ColumnType::*;
    let bytes = match v {
        Value::NULL => return Cell::Null,
        Value::Bytes(b) => b,
        Value::Int(i) => return Cell::Int(i),
        Value::UInt(u) => return i64::try_from(u).map(Cell::Int).unwrap_or(Cell::Dec(u.to_string())),
        Value::Float(f) => return Cell::Float(f as f64),
        Value::Double(f) => return Cell::Float(f),
        other => return Cell::Text(other.as_sql(true).trim_matches('\'').to_string()),
    };
    match t {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_INT24
        | MYSQL_TYPE_YEAR => {
            let s = String::from_utf8_lossy(&bytes);
            s.parse().map(Cell::Int).unwrap_or_else(|_| Cell::Dec(s.into_owned()))
        }
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => {
            let s = String::from_utf8_lossy(&bytes);
            s.parse().map(Cell::Float).unwrap_or_else(|_| Cell::Dec(s.into_owned()))
        }
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => Cell::Dec(String::from_utf8_lossy(&bytes).into_owned()),
        MYSQL_TYPE_DATE | MYSQL_TYPE_DATETIME | MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIME => {
            Cell::DateTime(String::from_utf8_lossy(&bytes).into_owned())
        }
        MYSQL_TYPE_BIT => Cell::Bytes(bytes),
        _ if binary => Cell::Bytes(bytes),
        _ => Cell::Text(String::from_utf8_lossy(&bytes).into_owned()),
    }
}

#[async_trait]
impl Session for MySession {
    fn dialect(&self) -> Dialect {
        Dialect::Mysql
    }
    fn cancel_flag(&self) -> &CancelFlag {
        &self.cancel
    }

    async fn run(&self, sql: &str, sink: &mut dyn RowSink) -> Result<RunOutcome> {
        self.cancel.reset();
        let mut conn = self.conn.lock().await;
        let mut result = conn.query_iter(sql).await?;
        let cols = result.columns();
        let mut out = RunOutcome::default();
        match cols.filter(|c| !c.is_empty()) {
            None => {
                out.affected = Some(result.affected_rows());
            }
            Some(cols) => {
                let kinds: Vec<(ColumnType, bool)> = cols
                    .iter()
                    .map(|c| {
                        let bin = c.character_set() == BINARY_CHARSET
                            && (c.flags().contains(ColumnFlags::BINARY_FLAG)
                                || matches!(
                                    c.column_type(),
                                    ColumnType::MYSQL_TYPE_BLOB
                                        | ColumnType::MYSQL_TYPE_TINY_BLOB
                                        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
                                        | ColumnType::MYSQL_TYPE_LONG_BLOB
                                        | ColumnType::MYSQL_TYPE_VAR_STRING
                                        | ColumnType::MYSQL_TYPE_STRING
                                ));
                        (c.column_type(), bin)
                    })
                    .collect();
                sink.columns(
                    cols.iter()
                        .map(|c| ColumnMeta::new(c.name_str(), type_name(c.column_type()), numeric(c.column_type())))
                        .collect(),
                )?;
                while let Some(row) = result.next().await? {
                    let cells = row
                        .unwrap()
                        .into_iter()
                        .zip(&kinds)
                        .map(|(v, (t, bin))| to_cell(v, *t, *bin))
                        .collect();
                    if !sink.row(cells)? || self.cancel.is_set() {
                        break;
                    }
                }
            }
        }
        drop(result);
        Ok(out)
    }

    async fn cancel(&self) -> Result<()> {
        self.cancel.set();
        let mut killer = Conn::new(self.opts.clone()).await?;
        killer.query_drop(format!("KILL QUERY {}", self.conn_id)).await?;
        killer.disconnect().await?;
        Ok(())
    }

    async fn set_autocommit(&self, on: bool) -> Result<()> {
        self.exec(if on { "SET autocommit=1" } else { "SET autocommit=0" }).await
    }

    async fn ping(&self) -> Result<()> {
        let Ok(mut conn) = self.conn.try_lock() else { return Ok(()) };
        conn.ping().await?;
        Ok(())
    }

    async fn server_version(&self) -> Result<String> {
        let mut conn = self.conn.lock().await;
        let v: Option<String> = conn.query_first("SELECT VERSION()").await?;
        Ok(v.unwrap_or_default())
    }
}
